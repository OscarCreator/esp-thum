use anyhow::{anyhow, bail, Result};
use std::str::Utf8Error;
// use chrono::{DateTime, Utc};
// use esp_idf_svc::sntp::{EspSntp, SyncStatus};
// use std::time::SystemTime;
use embedded_sht3x::{Measurement, Repeatability, Sht3x, TemperatureUnit, DEFAULT_I2C_ADDRESS};
use esp_idf_svc::hal::adc::attenuation::DB_11;
use esp_idf_svc::hal::adc::oneshot::config::{AdcChannelConfig, Calibration};
use esp_idf_svc::hal::adc::oneshot::{AdcChannelDriver, AdcDriver};
use esp_idf_svc::hal::adc::ADC1;
use esp_idf_svc::hal::gpio::{Gpio0, InputPin, OutputPin};
use esp_idf_svc::hal::i2c::{I2c, I2cError};
use esp_idf_svc::hal::peripheral::Peripheral;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, EspNvsPartition, NvsDefault};
use esp_idf_svc::sys::{esp_deep_sleep, esp_wifi_set_max_tx_power};
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::{
        delay,
        i2c::{I2cConfig, I2cDriver},
        peripheral,
        prelude::*,
    },
    mqtt::client::{EspMqttClient, MqttClientConfiguration, QoS},
};
use log::{error, info};
use serde::{Deserialize, Serialize};
use serde_json::json;

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/_uuid.rs"));

const NAME: &'static str = "thum";

#[derive(Debug)]
#[toml_cfg::toml_config]
pub struct Config {
    #[default("localhost")]
    mqtt_host: &'static str,
    #[default("")]
    mqtt_user: &'static str,
    #[default("")]
    mqtt_pass: &'static str,
    #[default("")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_psk: &'static str,
}

#[derive(Serialize, Deserialize)]
struct Availablity<'a> {
    topic: &'a str,
}

#[derive(Serialize, Deserialize)]
struct DiscoveryTopic<'a> {
    state_class: &'a str,
    state_topic: &'a str,
    unique_id: &'a str,
    name: &'a str,
    device_class: &'a str,
    device: DiscoveryDevice<'a>,
    unit_of_measurement: &'a str,
    qos: u8,
}

#[derive(Serialize, Deserialize)]
struct DiscoveryDevice<'a> {
    identifiers: &'a str,
    name: &'a str,
    model: &'a str,
}

fn get_sht_data(
    sda: impl Peripheral<P = impl InputPin + OutputPin>,
    scl: impl Peripheral<P = impl InputPin + OutputPin>,
    i2c: impl Peripheral<P = impl I2c>,
) -> core::result::Result<Measurement, embedded_sht3x::Error<I2cError>> {
    let config = I2cConfig::new().baudrate(100.kHz().into());
    // TODO handle errors
    let driver = I2cDriver::new(i2c, sda, scl, &config).unwrap();
    let mut temp_sensor = Sht3x::new(driver, DEFAULT_I2C_ADDRESS, delay::Ets);
    temp_sensor.repeatability = Repeatability::Medium;
    temp_sensor.unit = TemperatureUnit::Celcius;

    temp_sensor.single_measurement()
}

fn publish_sht_data(measurement: Measurement, client: &mut EspMqttClient) -> Result<()> {
    let topic = DiscoveryTopic {
        state_class: "measurement",
        state_topic: "thum/sensor/temperature/state",
        unique_id: &format!("{UUID}_temperature"),
        name: "Temperature",
        device_class: "temperature",
        device: DiscoveryDevice {
            identifiers: UUID,
            name: "thum",
            model: "esp32-c3",
        },
        unit_of_measurement: "°C",
        qos: 1,
    };

    client.publish(
        "homeassistant/sensor/thum/temperature/config",
        QoS::AtLeastOnce,
        true,
        serde_json::to_value(topic)?.to_string().as_bytes(),
    )?;

    let topic = DiscoveryTopic {
        state_class: "measurement",
        state_topic: "thum/sensor/humidity/state",
        unique_id: &format!("{UUID}_humidity"),
        name: "Humidity",
        device_class: "humidity",
        device: DiscoveryDevice {
            identifiers: UUID,
            name: "thum",
            model: "esp32-c3",
        },
        unit_of_measurement: "%",
        qos: 1,
    };

    let _r = client.publish(
        "homeassistant/sensor/thum/humidity/config",
        QoS::AtLeastOnce,
        true,
        serde_json::to_value(topic)?.to_string().as_bytes(),
    )?;

    let _r = client.publish(
        &format!("{NAME}/sensor/temperature/state"),
        QoS::AtLeastOnce,
        false,
        format!("{:.2}", measurement.temperature).as_bytes(),
    )?;
    let _r = client.publish(
        &format!("{NAME}/sensor/humidity/state"),
        QoS::AtLeastOnce,
        false,
        format!("{:.1}", measurement.humidity).as_bytes(),
    )?;
    Ok(())
}

// <impl Peripheral<P : Adc> as Peripheral>::P
fn get_voltage(pin: Gpio0, adc: ADC1) -> Result<u16> {
    let mut config = AdcChannelConfig::new();
    config.calibration = Calibration::Curve;
    config.attenuation = DB_11;
    let driver = AdcDriver::new(adc)?;
    let mut adc = AdcChannelDriver::new(driver, pin, &config)?;
    let output = adc.read_raw()?;
    let voltage = adc.raw_to_mv(output)?;

    info!("{}mV", voltage);
    Ok(voltage)
}

// TODO check time until next half hour
//{
//    let ntp = EspSntp::new_default()?;
//    while ntp.get_sync_status() != SyncStatus::Completed {}

//    let st_now = SystemTime::now();
//    let dt_now_utc: DateTime<Utc> = st_now.into();
//    let formatted = format!("{}", dt_now_utc.format("%d/%m/%Y %H:%M:%S"));
//    info!("Time: {}", formatted);

//    info!("UUID: {}", UUID);
//}
fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    let nvs_default_partition: EspNvsPartition<NvsDefault> =
        EspDefaultNvsPartition::take().unwrap();

    let nvs_clone = nvs_default_partition.clone();

    let test_namespace = "test_ns";
    let mut nvs = match EspNvs::new(nvs_default_partition, test_namespace, true) {
        Ok(nvs) => {
            info!("Got namespace {:?} from default partition", test_namespace);
            nvs
        }
        Err(e) => panic!("Could't get namespace {:?}", e),
    };

    let previous = "previous";
    let previous_data: &mut [u8] = &mut [0; 255];
    let mut prev_res: Result<&str, Utf8Error> = Ok("default");
    {
        match nvs.get_raw(previous, previous_data) {
            Ok(v) => match v {
                Some(vv) => {
                    info!("{:?} = {:?}", previous, str::from_utf8(vv));
                    prev_res = str::from_utf8(vv);
                }
                None => info!("empty nvs"),
            },
            Err(e) => info!("Couldn't get key {} because{:?}", previous, e),
        };
    }

    let res = main_2(prev_res, nvs_clone);
    match &res {
        Ok(_) => {}
        Err(err) => error!("error: {err}"),
    };

    {
        let res_format = format!("{:?}", &res);
        let key_raw_u8_data: &[u8] = res_format.as_bytes();

        match nvs.set_raw(previous, key_raw_u8_data) {
            Ok(_) => info!("Key updated with: {:?}", res),
            // You can find the meaning of the error codes in the output of the error branch in:
            // https://docs.espressif.com/projects/esp-idf/en/latest/esp32/api-reference/error-codes.html
            Err(e) => info!("Key not updated {:?}", e),
        };
    }

    unsafe {
        // 20min
        esp_deep_sleep(30 * 60 * 1_000_000);
    };
}

fn main_2(prev_res: Result<&str, Utf8Error>, nvs: EspNvsPartition<NvsDefault>) -> Result<()> {
    // TODO how to handle errors so they are logged
    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let pins = peripherals.pins;

    // The constant `CONFIG` is auto-generated by `toml_config`.
    let app_config = CONFIG;
    info!("config: {:?}", CONFIG);

    let measurement = get_sht_data(pins.gpio5, pins.gpio4, peripherals.i2c0)
        .map_err(|e| anyhow!("sht data error: {e:?}"))?;
    let voltage = get_voltage(pins.gpio0, peripherals.adc1)?;

    let wifi = wifi(
        app_config.wifi_ssid,
        app_config.wifi_psk,
        peripherals.modem,
        sysloop,
        nvs,
    )
    .map_err(|e| anyhow!("wifi: {e}"))?;

    // Client configuration:
    let broker_url = format!(
        "mqtt://{}:{}@{}",
        app_config.mqtt_user, app_config.mqtt_pass, app_config.mqtt_host
    );

    let mqtt_config = MqttClientConfiguration::default();

    let mut client = EspMqttClient::new_cb(&broker_url, &mqtt_config, |_| {})
        .map_err(|e| anyhow!("mqtt client: {e}"))?;

    let r = client
        .publish(
            "homeassistant/sensor/thum/result/config",
            QoS::AtLeastOnce,
            true,
            json!({
                "command_template": "Res: {{ value }}",
                "platform": "esp32-c3",
                "qos": 1,
                "unique_id": &format!("{UUID}_result"),
                "state_topic": "thum/sensor/result/state",
                "command_topic": "thum/sensor/result/state",
                "name": "Result",
                "device": DiscoveryDevice {
                    identifiers: UUID,
                    name: "thum",
                    model: "esp32-c3",
                },

            })
            .to_string()
            .as_bytes(),
        )
        .map_err(|e| anyhow!("p1: {e}"))?;
    info!("{r}");

    let r = client.publish(
        "thum/sensor/result/state",
        QoS::AtLeastOnce,
        false,
        format!("{prev_res:?}").as_bytes(),
    )?;
    info!("{r}");
    //wifi.is_up().map_err(|e| anyhow!("is_up1: {e}"))?;

    publish_sht_data(measurement, &mut client).map_err(|e| anyhow!("sht: {e}"))?;
    publish_rssi(wifi.get_rssi()?, &mut client).map_err(|e| anyhow!("rssi: {e}"))?;
    wifi.is_up().map_err(|e| anyhow!("is_up2: {e}"))?;

    let topic = DiscoveryTopic {
        state_class: "measurement",
        state_topic: "thum/sensor/voltage/state",
        unique_id: &format!("{UUID}_voltage"),
        name: "Voltage",
        device_class: "voltage",
        device: DiscoveryDevice {
            identifiers: UUID,
            name: "thum",
            model: "esp32-c3",
        },
        unit_of_measurement: "V",
        qos: 1,
    };

    let r = client
        .publish(
            "homeassistant/sensor/thum/voltage/config",
            QoS::AtLeastOnce,
            true,
            serde_json::to_value(topic)?.to_string().as_bytes(),
        )
        .map_err(|e| anyhow!("p2: {e}"))?;
    info!("{r}");

    let r = client
        .publish(
            "thum/sensor/voltage/state",
            QoS::AtLeastOnce,
            false,
            format!("{:.2}", f32::from(voltage) / 1000. * 2.).as_bytes(),
        )
        .map_err(|e| anyhow!("p3: {e}"))?;
    info!("{r}");
    //wifi.is_up().map_err(|e| anyhow!("is_up3: {e}"))?;
    //wifi.stop().map_err(|e| anyhow!("wifi stop: {e}"))?;
    Ok(())
}

fn publish_rssi(rssi: i32, client: &mut EspMqttClient<'_>) -> Result<()> {
    let topic = DiscoveryTopic {
        state_class: "measurement",
        state_topic: "thum/sensor/rssi/state",
        unique_id: &format!("{UUID}_rssi"),
        name: "Rssi",
        device_class: "rssi",
        device: DiscoveryDevice {
            identifiers: UUID,
            name: "thum",
            model: "esp32-c3",
        },
        unit_of_measurement: "dBm",
        qos: 1,
    };

    let _r = client.publish(
        "homeassistant/sensor/thum/rssi/config",
        QoS::AtLeastOnce,
        true,
        serde_json::to_value(topic)?.to_string().as_bytes(),
    )?;
    let _r = client
        .publish(
            "thum/sensor/rssi/state",
            QoS::AtLeastOnce,
            false,
            format!("{rssi}").as_bytes(),
        )
        .map_err(|e| anyhow!("state: {e}"))?;
    Ok(())
}

pub fn wifi(
    ssid: &str,
    pass: &str,
    modem: impl peripheral::Peripheral<P = esp_idf_svc::hal::modem::Modem> + 'static,
    sysloop: EspSystemEventLoop,
    nvs: EspNvsPartition<NvsDefault>,
) -> Result<Box<EspWifi<'static>>> {
    let mut auth_method = AuthMethod::WPA2Personal;
    if ssid.is_empty() {
        bail!("Missing WiFi name")
    }
    if pass.is_empty() {
        auth_method = AuthMethod::None;
        info!("Wifi password is empty");
    }
    let mut esp_wifi = EspWifi::new(modem, sysloop.clone(), Some(nvs))?;
    // lower tx power from 20dBm to 14dBm
    unsafe { esp_wifi_set_max_tx_power(10 * 4) };

    let mut wifi = BlockingWifi::wrap(&mut esp_wifi, sysloop)?;

    wifi.set_configuration(&Configuration::Client(ClientConfiguration::default()))?;
    //let a = Configuration::Client(ClientConfiguration::default());

    info!("Starting wifi...");

    wifi.start()?;

    info!("Scanning...");

    let ap_infos = wifi.scan()?;

    let ours = ap_infos.into_iter().find(|a| a.ssid == ssid);

    let channel = if let Some(ours) = ours {
        info!(
            "Found configured access point {} on channel {}",
            ssid, ours.channel
        );
        Some(ours.channel)
    } else {
        info!(
            "Configured access point {} not found during scanning, will go with unknown channel",
            ssid
        );
        None
    };

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: ssid
            .try_into()
            .expect("Could not parse the given SSID into WiFi config"),
        password: pass
            .try_into()
            .expect("Could not parse the given password into WiFi config"),
        channel,
        auth_method,
        ..Default::default()
    }))?;

    info!("Connecting wifi...");

    wifi.connect()?;

    info!("Waiting for DHCP lease...");

    wifi.wait_netif_up()?;

    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;

    info!("Wifi DHCP info: {:?}", ip_info);

    Ok(Box::new(esp_wifi))
}
