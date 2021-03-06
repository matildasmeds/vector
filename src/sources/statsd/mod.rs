use crate::{
    config::{self, GenerateConfig, GlobalOptions, SourceConfig, SourceDescription},
    internal_events::{StatsdEventReceived, StatsdInvalidRecord, StatsdSocketError},
    shutdown::ShutdownSignal,
    sources::util::{SocketListenAddr, TcpSource},
    tls::{MaybeTlsSettings, TlsConfig},
    Event, Pipeline,
};
use bytes::Bytes;
use codec::BytesDelimitedCodec;
use futures::{compat::Sink01CompatExt, stream, FutureExt, SinkExt, StreamExt, TryFutureExt};
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use tokio::net::UdpSocket;
use tokio_util::{codec::BytesCodec, udp::UdpFramed};

pub mod parser;
#[cfg(unix)]
mod unix;

use parser::parse;
#[cfg(unix)]
use unix::{statsd_unix, UnixConfig};

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum StatsdConfig {
    Tcp(TcpConfig),
    Udp(UdpConfig),
    #[cfg(unix)]
    Unix(UnixConfig),
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct UdpConfig {
    pub address: SocketAddr,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct TcpConfig {
    address: SocketListenAddr,
    #[serde(default)]
    tls: Option<TlsConfig>,
    #[serde(default = "default_shutdown_timeout_secs")]
    pub shutdown_timeout_secs: u64,
}

fn default_shutdown_timeout_secs() -> u64 {
    30
}

inventory::submit! {
    SourceDescription::new::<StatsdConfig>("statsd")
}

impl GenerateConfig for StatsdConfig {
    fn generate_config() -> toml::Value {
        toml::Value::try_from(Self::Udp(UdpConfig {
            address: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8125)),
        }))
        .unwrap()
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "statsd")]
impl SourceConfig for StatsdConfig {
    async fn build(
        &self,
        _name: &str,
        _globals: &GlobalOptions,
        shutdown: ShutdownSignal,
        out: Pipeline,
    ) -> crate::Result<super::Source> {
        match self {
            StatsdConfig::Udp(config) => Ok(Box::new(
                statsd_udp(config.clone(), shutdown, out).boxed().compat(),
            )),
            StatsdConfig::Tcp(config) => {
                let tls = MaybeTlsSettings::from_config(&config.tls, true)?;
                StatsdTcpSource.run(
                    config.address,
                    config.shutdown_timeout_secs,
                    tls,
                    shutdown,
                    out,
                )
            }
            #[cfg(unix)]
            StatsdConfig::Unix(config) => Ok(statsd_unix(config.clone(), shutdown, out)),
        }
    }

    fn output_type(&self) -> crate::config::DataType {
        config::DataType::Metric
    }

    fn source_type(&self) -> &'static str {
        "statsd"
    }
}

pub(self) fn parse_event(line: &str) -> Option<Event> {
    match parse(line) {
        Ok(metric) => {
            emit!(StatsdEventReceived {
                byte_size: line.len()
            });
            Some(Event::Metric(metric))
        }
        Err(error) => {
            emit!(StatsdInvalidRecord { error, text: line });
            None
        }
    }
}

async fn statsd_udp(config: UdpConfig, shutdown: ShutdownSignal, out: Pipeline) -> Result<(), ()> {
    let socket = UdpSocket::bind(&config.address)
        .map_err(|error| emit!(StatsdSocketError::bind(error)))
        .await?;

    info!(
        message = "Listening.",
        addr = %config.address,
        r#type = "udp"
    );

    let mut stream = UdpFramed::new(socket, BytesCodec::new()).take_until(shutdown);
    let mut out = out.sink_compat();
    while let Some(frame) = stream.next().await {
        match frame {
            Ok((bytes, _sock)) => {
                let packet = String::from_utf8_lossy(bytes.as_ref());
                let metrics = packet.lines().filter_map(parse_event).map(Ok);

                // Need `boxed` to resolve a lifetime issue
                // https://github.com/rust-lang/rust/issues/64552#issuecomment-669728225
                let mut metrics = stream::iter(metrics).boxed();
                if let Err(error) = out.send_all(&mut metrics).await {
                    error!("Error sending metric: {:?}", error);
                    break;
                }
            }
            Err(error) => {
                emit!(StatsdSocketError::read(error));
            }
        }
    }

    Ok(())
}

#[derive(Clone)]
struct StatsdTcpSource;

impl TcpSource for StatsdTcpSource {
    type Error = std::io::Error;
    type Decoder = BytesDelimitedCodec;

    fn decoder(&self) -> Self::Decoder {
        BytesDelimitedCodec::new(b'\n')
    }

    fn build_event(&self, line: Bytes, _host: Bytes) -> Option<Event> {
        let line = String::from_utf8_lossy(line.as_ref());
        parse_event(&line)
    }
}

#[cfg(feature = "sinks-prometheus")]
#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        config,
        sinks::prometheus::PrometheusSinkConfig,
        test_util::{next_addr, start_topology},
    };
    use futures::{compat::Future01CompatExt, TryStreamExt};
    use futures01::Stream;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::mpsc;
    use tokio::time::{delay_for, Duration};

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<StatsdConfig>();
    }

    fn parse_count(lines: &[&str], prefix: &str) -> usize {
        lines
            .iter()
            .find(|s| s.starts_with(prefix))
            .map(|s| s.split_whitespace().nth(1).unwrap())
            .unwrap()
            .parse::<usize>()
            .unwrap()
    }

    #[tokio::test]
    async fn test_statsd_udp() {
        let in_addr = next_addr();
        let config = StatsdConfig::Udp(UdpConfig { address: in_addr });
        let sender = {
            let (sender, mut receiver) = mpsc::channel(200);
            let addr = in_addr;
            tokio::spawn(async move {
                let bind_addr = next_addr();
                let mut socket = UdpSocket::bind(bind_addr).await.unwrap();
                socket.connect(addr).await.unwrap();
                while let Some(bytes) = receiver.recv().await {
                    socket.send(bytes).await.unwrap();
                }
            });
            sender
        };
        test_statsd(config, sender).await;
    }

    #[tokio::test]
    async fn test_statsd_tcp() {
        let in_addr = next_addr();
        let config = StatsdConfig::Tcp(TcpConfig {
            address: in_addr.into(),
            tls: None,
            shutdown_timeout_secs: 30,
        });
        let sender = {
            let (sender, mut receiver) = mpsc::channel(200);
            let addr = in_addr;
            tokio::spawn(async move {
                while let Some(bytes) = receiver.recv().await {
                    tokio::net::TcpStream::connect(addr)
                        .await
                        .unwrap()
                        .write_all(bytes)
                        .await
                        .unwrap();
                }
            });
            sender
        };
        test_statsd(config, sender).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_statsd_unix() {
        let in_path = tempfile::tempdir().unwrap().into_path().join("unix_test");
        let config = StatsdConfig::Unix(UnixConfig {
            path: in_path.clone(),
        });
        let sender = {
            let (sender, mut receiver) = mpsc::channel(200);
            let path = in_path;
            tokio::spawn(async move {
                while let Some(bytes) = receiver.recv().await {
                    tokio::net::UnixStream::connect(&path)
                        .await
                        .unwrap()
                        .write_all(bytes)
                        .await
                        .unwrap();
                }
            });
            sender
        };
        test_statsd(config, sender).await;
    }

    async fn test_statsd(
        statsd_config: StatsdConfig,
        // could use unbounded channel,
        // but we want to reserve the order messages.
        mut sender: mpsc::Sender<&'static [u8]>,
    ) {
        let out_addr = next_addr();

        let mut config = config::Config::builder();
        config.add_source("in", statsd_config);
        config.add_sink(
            "out",
            &["in"],
            PrometheusSinkConfig {
                address: out_addr,
                namespace: Some("vector".into()),
                buckets: vec![1.0, 2.0, 4.0],
                quantiles: vec![],
                flush_period_secs: 1,
            },
        );

        let (topology, _crash) = start_topology(config.build().unwrap(), false).await;

        // Give some time for the topology to start
        delay_for(Duration::from_millis(100)).await;

        for _ in 0..100 {
            sender.send(
                b"foo:1|c|#a,b:b\nbar:42|g\nfoo:1|c|#a,b:c\nglork:3|h|@0.1\nmilliglork:3000|ms|@0.1\nset:0|s\nset:1|s\n"
            ).await.unwrap();
            // Space things out slightly to try to avoid dropped packets
            delay_for(Duration::from_millis(10)).await;
        }

        // Give packets some time to flow through
        delay_for(Duration::from_millis(100)).await;

        let client = hyper::Client::new();
        let response = client
            .get(format!("http://{}/metrics", out_addr).parse().unwrap())
            .await
            .unwrap();
        assert!(response.status().is_success());

        let body = response
            .into_body()
            .compat()
            .map(|bytes| bytes.to_vec())
            .concat2()
            .compat()
            .await
            .unwrap();
        let lines = std::str::from_utf8(&body)
            .unwrap()
            .lines()
            .collect::<Vec<_>>();

        // note that prometheus client reorders the labels
        let vector_foo1 = parse_count(&lines, "vector_foo{a=\"true\",b=\"b\"");
        let vector_foo2 = parse_count(&lines, "vector_foo{a=\"true\",b=\"c\"");
        // packets get lost :(
        assert!(vector_foo1 > 90);
        assert!(vector_foo2 > 90);

        let vector_bar = parse_count(&lines, "vector_bar");
        assert_eq!(42, vector_bar);

        assert_eq!(parse_count(&lines, "vector_glork_bucket{le=\"1\"}"), 0);
        assert_eq!(parse_count(&lines, "vector_glork_bucket{le=\"2\"}"), 0);
        assert!(parse_count(&lines, "vector_glork_bucket{le=\"4\"}") > 0);
        assert!(parse_count(&lines, "vector_glork_bucket{le=\"+Inf\"}") > 0);
        let glork_sum = parse_count(&lines, "vector_glork_sum");
        let glork_count = parse_count(&lines, "vector_glork_count");
        assert_eq!(glork_count * 3, glork_sum);

        assert_eq!(parse_count(&lines, "vector_milliglork_bucket{le=\"1\"}"), 0);
        assert_eq!(parse_count(&lines, "vector_milliglork_bucket{le=\"2\"}"), 0);
        assert!(parse_count(&lines, "vector_milliglork_bucket{le=\"4\"}") > 0);
        assert!(parse_count(&lines, "vector_milliglork_bucket{le=\"+Inf\"}") > 0);
        let milliglork_sum = parse_count(&lines, "vector_milliglork_sum");
        let milliglork_count = parse_count(&lines, "vector_milliglork_count");
        assert_eq!(milliglork_count * 3, milliglork_sum);

        // Set test
        // Flush could have occurred
        assert!(parse_count(&lines, "vector_set") <= 2);

        // Flush test
        {
            // Wait for flush to happen
            delay_for(Duration::from_millis(2000)).await;

            let response = client
                .get(format!("http://{}/metrics", out_addr).parse().unwrap())
                .await
                .unwrap();
            assert!(response.status().is_success());

            let body = response
                .into_body()
                .compat()
                .map(|bytes| bytes.to_vec())
                .concat2()
                .compat()
                .await
                .unwrap();
            let lines = std::str::from_utf8(&body)
                .unwrap()
                .lines()
                .collect::<Vec<_>>();

            // Check rested
            assert_eq!(parse_count(&lines, "vector_set"), 0);

            // Re-check that set is also reset------------

            sender.send(b"set:0|s\nset:1|s\n").await.unwrap();
            // Give packets some time to flow through
            delay_for(Duration::from_millis(100)).await;

            let response = client
                .get(format!("http://{}/metrics", out_addr).parse().unwrap())
                .await
                .unwrap();
            assert!(response.status().is_success());

            let body = response
                .into_body()
                .compat()
                .map(|bytes| bytes.to_vec())
                .concat2()
                .compat()
                .await
                .unwrap();
            let lines = std::str::from_utf8(&body)
                .unwrap()
                .lines()
                .collect::<Vec<_>>();

            // Set test
            assert_eq!(parse_count(&lines, "vector_set"), 2);
        }

        // Shut down server
        topology.stop().compat().await.unwrap();
    }
}
