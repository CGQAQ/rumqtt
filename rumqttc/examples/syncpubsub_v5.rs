use rumqttc::v5::mqttbytes::{LastWill, QoS};
use rumqttc::v5::{sync::Client};
use rumqttc::v5::ConnectionError;
use rumqttc::v5::{MqttOptions};
use std::thread;
use std::time::Duration;

fn main() {
    pretty_env_logger::init();

    let mut mqttoptions = MqttOptions::new("test-1", "localhost", 1884);
    let will = LastWill::new("hello/world", "good bye", QoS::AtMostOnce, false);
    mqttoptions
        .set_keep_alive(Duration::from_secs(5))
        .set_last_will(will);

    let (client, mut connection) = Client::new(mqttoptions, 10);
    thread::spawn(move || publish(client));

    for (i, notification) in connection.iter().enumerate() {
        match notification {
            Err(ConnectionError::Io(error))
                if error.kind() == std::io::ErrorKind::ConnectionRefused =>
            {
                println!("Failed to connect to the server. Make sure correct client is configured properly!\nError: {:?}", error);
                return;
            }
            _ => {}
        }
        println!("{}. Notification = {:?}", i, notification);
        if let Err(ref err) = notification {
            match err {
                rumqttc::v5::ConnectionError::Io(ref e) => {
                    if e.kind() == std::io::ErrorKind::ConnectionRefused {
                        // we can just stop the client!
                        // panic!("Please make sure broker is running and you are connecting to correct port!");
                        eprintln!("Please make sure broker is running and you are connecting to correct port!");
                        thread::sleep(Duration::from_secs(2));
                    };
                }
                // this should be error variant if connection is refused ???
                // rumqttc::v5::ConnectionError::ConnectionRefused(_) => todo!(),
                _ => (),
            }
        }
    }

    println!("Done with the stream!!");
}

fn publish(client: Client) {
    client
        .subscribe("hello/+/world", QoS::AtMostOnce)
        .expect("subscribe to topic");

    for i in 0..10_usize {
        let payload = vec![1; i];
        let topic = format!("hello/{}/world", i);
        let qos = QoS::AtLeastOnce;

<<<<<<< HEAD
        client
            .publisher(topic)
            .publish(qos, true, payload)
            .expect("publised message")
=======
        let _ = client.publish(topic, qos, true, payload);
>>>>>>> main
    }

    thread::sleep(Duration::from_secs(1));
}
