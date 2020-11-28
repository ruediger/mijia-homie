use crate::decode::decode_temperature;
use std::cmp::max;
use std::convert::TryInto;
use std::fmt::{self, Display, Formatter};

/// A set of readings from a Mijia sensor.
#[derive(Clone, Debug, PartialEq)]
pub struct Readings {
    /// Temperature in ºC, with 2 decimal places of precision
    pub temperature: f32,
    /// Percent humidity
    pub humidity: u8,
    /// Voltage in millivolts
    pub battery_voltage: u16,
    /// Inferred from `battery_voltage` with a bit of hand-waving.
    pub battery_percent: u16,
}

impl Display for Readings {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(
            f,
            "Temperature: {:.2}ºC Humidity: {:?}% Battery: {:?} mV ({:?}%)",
            self.temperature, self.humidity, self.battery_voltage, self.battery_percent
        )
    }
}

impl Readings {
    /// Decode the readings from the raw bytes of the Bluetooth characteristic value, if they are
    /// valid.
    /// Returns `None` if the value is not valid.
    pub(crate) fn decode(value: &[u8]) -> Option<Readings> {
        if value.len() != 5 {
            return None;
        }

        let mut temperature_array = [0; 2];
        temperature_array.clone_from_slice(&value[..2]);
        let temperature = decode_temperature(temperature_array);
        let humidity = value[2];
        let battery_voltage = u16::from_le_bytes(value[3..5].try_into().unwrap());
        let battery_percent = (max(battery_voltage, 2100) - 2100) / 10;
        Some(Readings {
            temperature,
            humidity,
            battery_voltage,
            battery_percent,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_empty() {
        assert_eq!(Readings::decode(&[]), None);
    }

    #[test]
    fn decode_too_short() {
        assert_eq!(Readings::decode(&[1, 2, 3, 4]), None);
    }

    #[test]
    fn decode_too_long() {
        assert_eq!(Readings::decode(&[1, 2, 3, 4, 5, 6]), None);
    }

    #[test]
    fn decode_valid() {
        assert_eq!(
            Readings::decode(&[1, 2, 3, 4, 10]),
            Some(Readings {
                temperature: 5.13,
                humidity: 3,
                battery_voltage: 2564,
                battery_percent: 46
            })
        );
    }
}
