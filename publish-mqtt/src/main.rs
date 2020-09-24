use anyhow::Context;
use futures::stream::StreamExt;
use futures::{FutureExt, TryFutureExt};
use homie_device::{Datatype, HomieDevice, Node, Property};
use itertools::Itertools;
use mijia::{DeviceId, MacAddress, MijiaEvent, MijiaSession, Readings, SensorProps};
use rumqttc::MqttOptions;
use rustls::ClientConfig;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::{task, time, try_join};

const DEFAULT_MQTT_PREFIX: &str = "homie";
const DEFAULT_DEVICE_ID: &str = "mijia-bridge";
const DEFAULT_DEVICE_NAME: &str = "Mijia bridge";
const DEFAULT_HOST: &str = "test.mosquitto.org";
const DEFAULT_PORT: u16 = 1883;
const SCAN_INTERVAL: Duration = Duration::from_secs(15);
const CONNECT_INTERVAL: Duration = Duration::from_secs(1);
const UPDATE_TIMEOUT: Duration = Duration::from_secs(60);
const SENSOR_CONNECT_RESERVATION_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const SENSOR_NAMES_FILENAME: &str = "sensor_names.conf";

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    dotenv::dotenv().context("reading .env")?;
    pretty_env_logger::init();
    color_backtrace::install();

    let device_id = std::env::var("DEVICE_ID").unwrap_or_else(|_| DEFAULT_DEVICE_ID.to_string());
    let device_name =
        std::env::var("DEVICE_NAME").unwrap_or_else(|_| DEFAULT_DEVICE_NAME.to_string());
    let client_name = std::env::var("CLIENT_NAME").unwrap_or_else(|_| device_id.clone());

    let host = std::env::var("HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string());

    let port = std::env::var("PORT")
        .ok()
        .and_then(|val| val.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);

    let mut mqttoptions = MqttOptions::new(client_name, host, port);

    let username = std::env::var("USERNAME").ok();
    let password = std::env::var("PASSWORD").ok();

    mqttoptions.set_keep_alive(5);
    if let (Some(u), Some(p)) = (username, password) {
        mqttoptions.set_credentials(u, p);
    }

    // Use `env -u USE_TLS` to unset this variable if you need to clear it.
    if std::env::var("USE_TLS").is_ok() {
        let mut client_config = ClientConfig::new();
        client_config.root_store =
            rustls_native_certs::load_native_certs().expect("could not load platform certs");
        mqttoptions.set_tls_client_config(Arc::new(client_config));
    }

    let mqtt_prefix =
        std::env::var("MQTT_PREFIX").unwrap_or_else(|_| DEFAULT_MQTT_PREFIX.to_string());
    let device_base = format!("{}/{}", mqtt_prefix, device_id);
    let (homie, homie_handle) = HomieDevice::builder(&device_base, &device_name, mqttoptions)
        .spawn()
        .await?;

    let local = task::LocalSet::new();

    // Connect a bluetooth session.
    let (dbus_handle, session) = MijiaSession::new().await?;

    let sensor_handle = local.run_until(async move { run_sensor_system(homie, &session).await });

    // Poll everything to completion, until the first one bombs out.
    let res: Result<_, anyhow::Error> = try_join! {
        // If this ever finishes, we lost connection to D-Bus.
        dbus_handle,
        // Bluetooth finished first. Convert error and get on with your life.
        sensor_handle.map(|res| Ok(res?)),
        // MQTT event loop finished first.
        homie_handle.map_err(|err| anyhow::anyhow!(err)),
    };
    res?;
    Ok(())
}

#[derive(Debug, Copy, Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum ConnectionStatus {
    /// Not yet attempted to connect. Might already be connected from a previous
    /// run of this program.
    Unknown,
    /// Currently connecting. Don't try again until the timeout expires.
    Connecting { reserved_until: Instant },
    /// Connected, but could not subscribe to updates. GATT characteristics
    /// sometimes take a while to show up after connecting, so this retry is
    /// a bit of a work-around.
    SubscribingFailedOnce,
    /// Disconnected because we stopped getting updates.
    WatchdogTimeOut,
    /// We explicity disconnected because something else went wrong.
    Disconnected,
    /// We received a Disconnected event.
    /// This should only be treated as informational, because disconnection
    /// events might be received racily. The sensor might actually be Connected.
    MarkedDisconnected,
    /// Connected and subscribed to updates
    Connected,
}

#[derive(Debug, Clone)]
struct Sensor {
    id: DeviceId,
    mac_address: MacAddress,
    name: String,
    last_update_timestamp: Instant,
    connection_status: ConnectionStatus,
}

impl Sensor {
    const PROPERTY_ID_TEMPERATURE: &'static str = "temperature";
    const PROPERTY_ID_HUMIDITY: &'static str = "humidity";
    const PROPERTY_ID_BATTERY: &'static str = "battery";

    pub fn new(props: SensorProps, sensor_names: &HashMap<MacAddress, String>) -> Self {
        let name = sensor_names
            .get(&props.mac_address)
            .cloned()
            .unwrap_or_else(|| props.mac_address.to_string());
        Self {
            id: props.id,
            mac_address: props.mac_address,
            name,
            last_update_timestamp: Instant::now(),
            connection_status: ConnectionStatus::Unknown,
        }
    }

    pub fn node_id(&self) -> String {
        self.mac_address.to_string().replace(":", "")
    }

    fn as_node(&self) -> Node {
        Node::new(
            &self.node_id(),
            &self.name,
            "Mijia sensor",
            vec![
                Property::new(
                    Self::PROPERTY_ID_TEMPERATURE,
                    "Temperature",
                    Datatype::Float,
                    false,
                    Some("ºC"),
                    None,
                ),
                Property::new(
                    Self::PROPERTY_ID_HUMIDITY,
                    "Humidity",
                    Datatype::Integer,
                    false,
                    Some("%"),
                    None,
                ),
                Property::new(
                    Self::PROPERTY_ID_BATTERY,
                    "Battery level",
                    Datatype::Integer,
                    false,
                    Some("%"),
                    None,
                ),
            ],
        )
    }

    async fn publish_readings(
        &mut self,
        homie: &HomieDevice,
        readings: &Readings,
    ) -> Result<(), anyhow::Error> {
        println!("{} {} ({})", self.mac_address, readings, self.name);

        let node_id = self.node_id();
        self.last_update_timestamp = Instant::now();
        homie
            .publish_value(
                &node_id,
                Self::PROPERTY_ID_TEMPERATURE,
                format!("{:.2}", readings.temperature),
            )
            .await
            .with_context(|| std::line!().to_string())?;
        homie
            .publish_value(&node_id, Self::PROPERTY_ID_HUMIDITY, readings.humidity)
            .await
            .with_context(|| std::line!().to_string())?;
        homie
            .publish_value(
                &node_id,
                Self::PROPERTY_ID_BATTERY,
                readings.battery_percent,
            )
            .await
            .with_context(|| std::line!().to_string())?;
        Ok(())
    }

    async fn mark_connected(&mut self, homie: &mut HomieDevice) -> Result<(), anyhow::Error> {
        homie
            .add_node(self.as_node())
            .await
            .with_context(|| std::line!().to_string())?;
        self.connection_status = ConnectionStatus::Connected;
        Ok(())
    }
}

async fn run_sensor_system(
    mut homie: HomieDevice,
    session: &MijiaSession,
) -> Result<(), anyhow::Error> {
    let sensor_names = hashmap_from_file(SENSOR_NAMES_FILENAME)
        .context(format!("reading {}", SENSOR_NAMES_FILENAME))?;

    homie
        .ready()
        .await
        .with_context(|| std::line!().to_string())?;

    let state = Arc::new(Mutex::new(SensorState {
        sensors: vec![],
        next_idx: 0,
        homie,
    }));

    let connection_loop_handle = bluetooth_connection_loop(state.clone(), session, &sensor_names);
    let event_loop_handle = service_bluetooth_event_queue(state.clone(), session);
    try_join!(connection_loop_handle, event_loop_handle).map(|((), ())| ())
}

/// Read the given file of key-value pairs into a hashmap.
/// Returns an empty hashmap if the file doesn't exist, or an error if it is malformed.
pub fn hashmap_from_file(filename: &str) -> Result<HashMap<MacAddress, String>, anyhow::Error> {
    let mut map: HashMap<MacAddress, String> = HashMap::new();
    if let Ok(file) = File::open(filename) {
        for line in BufReader::new(file).lines() {
            let line = line?;
            let parts: Vec<&str> = line.splitn(2, '=').collect();
            if parts.len() != 2 {
                anyhow::bail!("Invalid line '{}'", line);
            }
            map.insert(parts[0].parse()?, parts[1].to_string());
        }
    }
    Ok(map)
}

async fn bluetooth_connection_loop(
    state: Arc<Mutex<SensorState>>,
    session: &MijiaSession,
    sensor_names: &HashMap<MacAddress, String>,
) -> Result<(), anyhow::Error> {
    let mut next_scan_due = Instant::now();
    loop {
        {
            let counts = state
                .lock()
                .await
                .sensors
                .iter()
                .map(|sensor| (sensor.connection_status, sensor.name.clone()))
                .into_group_map();
            for (state, names) in counts.iter().sorted() {
                println!("{:?}: {} {:?}", state, names.len(), names);
            }
        }

        let now = Instant::now();
        if now > next_scan_due && state.lock().await.sensors.len() < sensor_names.len() {
            next_scan_due = now + SCAN_INTERVAL;
            check_for_sensors(state.clone(), session, &sensor_names)
                .await
                .with_context(|| std::line!().to_string())?;
        }

        {
            // TODO: Iterate over sensors here rather than storing next_idx in SensorState.
            action_next_sensor(state.clone(), session.clone())
                .await
                .with_context(|| std::line!().to_string())?;
        }
        time::delay_for(CONNECT_INTERVAL).await;
    }
}

#[derive(Debug)]
struct SensorState {
    sensors: Vec<Sensor>,
    next_idx: usize,
    homie: HomieDevice,
}

async fn action_next_sensor(
    state: Arc<Mutex<SensorState>>,
    session: &MijiaSession,
) -> Result<(), anyhow::Error> {
    let (idx, status) = match next_actionable_sensor(state.clone()).await {
        Some(values) => values,
        None => return Ok(()),
    };
    {
        let sensor = &state.lock().await.sensors[idx];
        println!("State of {} is {:?}", sensor.name, status);
    }
    match status {
        ConnectionStatus::Connecting { reserved_until } if reserved_until > Instant::now() => {
            Ok(())
        }
        ConnectionStatus::Unknown
        | ConnectionStatus::Connecting { .. }
        | ConnectionStatus::SubscribingFailedOnce
        | ConnectionStatus::Disconnected
        | ConnectionStatus::MarkedDisconnected
        | ConnectionStatus::WatchdogTimeOut => {
            connect_sensor_at_idx(state, session, idx).await?;
            Ok(())
        }
        ConnectionStatus::Connected => {
            check_for_stale_sensor(state, session, idx).await?;
            Ok(())
        }
    }
}

// TODO: If we make sensors in the state Vec<Arc<Mutex<Sensor>>>, then this can return the Arc<Mutex<Sensor>> rather than the index.
async fn next_actionable_sensor(
    state: Arc<Mutex<SensorState>>,
) -> Option<(usize, ConnectionStatus)> {
    let mut state = &mut *state.lock().await;
    let idx = state.next_idx;

    match state.sensors.get(idx) {
        None => {
            state.next_idx = 0;
            None
        }
        Some(sensor) => {
            state.next_idx += 1;
            Some((idx, sensor.connection_status))
        }
    }
}

async fn clone_sensor_at_idx(state: Arc<Mutex<SensorState>>, idx: usize) -> Sensor {
    state.lock().await.sensors[idx].clone()
}

async fn check_for_sensors(
    state: Arc<Mutex<SensorState>>,
    session: &MijiaSession,
    sensor_names: &HashMap<MacAddress, String>,
) -> Result<(), anyhow::Error> {
    session.bt_session.start_discovery().await?;

    let sensors = session
        .get_sensors()
        .await
        .with_context(|| std::line!().to_string())?;
    let state = &mut *state.lock().await;
    for props in sensors {
        if sensor_names.contains_key(&props.mac_address)
            && !state
                .sensors
                .iter()
                .find(|s| s.mac_address == props.mac_address)
                .is_some()
        {
            state.sensors.push(Sensor::new(props, &sensor_names))
        }
    }
    Ok(())
}

async fn connect_sensor_at_idx(
    state: Arc<Mutex<SensorState>>,
    session: &MijiaSession,
    idx: usize,
) -> Result<(), anyhow::Error> {
    let mut sensor = clone_sensor_at_idx(state.clone(), idx).await;
    {
        state.lock().await.sensors[idx].connection_status = ConnectionStatus::Connecting {
            reserved_until: Instant::now() + SENSOR_CONNECT_RESERVATION_TIMEOUT,
        }
    }
    // Try to connect to a sensor.
    println!("Trying to connect to {}", sensor.name);
    let status = connect_start_sensor(session, &mut sensor).await;
    let mut state = state.lock().await;
    match status {
        Err(e) => {
            println!(
                "Failed to connect to {} (now {:?}): {:?}",
                sensor.name, sensor.connection_status, e
            );
        }
        Ok(()) => {
            println!("Connected to {} and started notifications", sensor.name);
            sensor.mark_connected(&mut state.homie).await?;
            sensor.last_update_timestamp = Instant::now();
        }
    }
    state.sensors[idx] = sensor;

    Ok(())
}

async fn connect_start_sensor<'a>(
    session: &MijiaSession,
    sensor: &mut Sensor,
) -> Result<(), anyhow::Error> {
    println!("Connecting from status: {:?}", sensor.connection_status);
    session
        .bt_session
        .connect(&sensor.id)
        .await
        .with_context(|| std::line!().to_string())?;
    match session.start_notify_sensor(&sensor.id).await {
        Ok(()) => Ok(()),
        Err(e) => {
            // If starting notifications failed a second time, disconnect so
            // that we start again from a clean state next time.
            match sensor.connection_status {
                ConnectionStatus::Unknown
                | ConnectionStatus::Connecting { .. }
                | ConnectionStatus::Disconnected
                | ConnectionStatus::MarkedDisconnected
                | ConnectionStatus::WatchdogTimeOut => {
                    sensor.connection_status = ConnectionStatus::SubscribingFailedOnce;
                }
                ConnectionStatus::SubscribingFailedOnce => {
                    session
                        .bt_session
                        .disconnect(&sensor.id)
                        .await
                        .with_context(|| std::line!().to_string())?;
                    sensor.connection_status = ConnectionStatus::Disconnected;
                }
                ConnectionStatus::Connected => panic!("This should never happen."),
            };
            Err(e)
        }
    }
}

/// If the sensor hasn't sent any updates in a while, disconnect it so we will try to reconnect.
async fn check_for_stale_sensor(
    state: Arc<Mutex<SensorState>>,
    session: &MijiaSession,
    idx: usize,
) -> Result<(), anyhow::Error> {
    let state = &mut *state.lock().await;
    let sensor = &mut state.sensors[idx];
    let now = Instant::now();
    if now - sensor.last_update_timestamp > UPDATE_TIMEOUT {
        println!(
            "No update from {} for {:?}, reconnecting",
            sensor.name,
            now - sensor.last_update_timestamp
        );
        // TODO: Should we disconnect the device first?
        sensor.connection_status = ConnectionStatus::WatchdogTimeOut;
        state
            .homie
            .remove_node(&sensor.node_id())
            .await
            .with_context(|| std::line!().to_string())?;
    }
    Ok(())
}

async fn service_bluetooth_event_queue(
    state: Arc<Mutex<SensorState>>,
    session: &MijiaSession,
) -> Result<(), anyhow::Error> {
    println!("Subscribing to events");
    let (msg_match, mut events) = session.event_stream().await?;
    println!("Processing events");

    while let Some(event) = events.next().await {
        handle_bluetooth_event(state.clone(), event)
            .await
            .with_context(|| std::line!().to_string())?
    }

    session
        .bt_session
        .connection
        .remove_match(msg_match.token())
        .await?;
    // This should be unreachable, because the events Stream should never end,
    // unless something has gone horribly wrong (or msg_match got dropped?)
    panic!("no more events");
}

async fn handle_bluetooth_event(
    state: Arc<Mutex<SensorState>>,
    event: MijiaEvent,
) -> Result<(), anyhow::Error> {
    let state = &mut *state.lock().await;
    let homie = &mut state.homie;
    let sensors = &mut state.sensors;
    match event {
        MijiaEvent::Readings { id, readings } => {
            if let Some(sensor) = sensors.iter_mut().find(|s| s.id == id) {
                sensor.publish_readings(homie, &readings).await?;
                match sensor.connection_status {
                    ConnectionStatus::Connected | ConnectionStatus::Connecting { .. } => {}
                    _ => {
                        println!("Got update from disconnected device {:?}. Connecting.", id);
                        sensor.mark_connected(homie).await?;
                        // TODO: Make sure the connection interval is set.
                    }
                }
            } else {
                println!("Got update from unknown device {:?}.", id);
            }
        }
        MijiaEvent::Disconnected { id } => {
            if let Some(sensor) = sensors.iter_mut().find(|s| s.id == id) {
                if sensor.connection_status == ConnectionStatus::Connected {
                    println!("{} disconnected", sensor.name);
                    sensor.connection_status = ConnectionStatus::MarkedDisconnected;
                    homie.remove_node(&sensor.node_id()).await?;
                } else {
                    println!("{:?} disconnected but wasn't known to be connected.", id);
                }
            } else {
                println!("Unknown device {:?} disconnected.", id);
            }
        }
    };

    Ok(())
}
