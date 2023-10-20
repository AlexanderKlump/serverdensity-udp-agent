use crate::processor::InboundMetric;
use crate::serverdensity::{AverageHandler, MinHandler, PeakHandler, SumHandler};
use clap::ArgMatches;
use crossbeam_channel::{Receiver, TryRecvError};
use openmetrics_udpserver_lib::MetricType;
use regex::Regex;
use reqwest::blocking::Client;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::thread;
use std::time::{Duration, SystemTime};

#[derive(Clone)]
pub struct ServerDensityConfig {
    pub token: String,
    pub account_url: String,
    pub agent_key: String,
    pub serverdensity_endpoint: String,
}

impl ServerDensityConfig {
    pub fn from_args(matches: ArgMatches) -> Self {
        let mut base_config = ServerDensityConfig {
            token: matches.get_one::<String>("token").unwrap().to_string(),
            account_url: matches
                .get_one::<String>("account-url")
                .unwrap_or(&"".to_string())
                .to_string(),
            agent_key: matches
                .get_one::<String>("agent-key")
                .unwrap_or(&"".to_string())
                .to_string(),
            serverdensity_endpoint: matches
                .get_one::<String>("serverdensity-endpoint")
                .unwrap()
                .to_string(),
        };

        if matches.get_one::<String>("config").is_some() {
            let config_file = matches.get_one::<String>("config").unwrap().to_string();
            match base_config.apply_config_file(&config_file) {
                Ok(_) => println!("successfully read config_file: {}", &config_file),
                Err(_) => {
                    panic!("could not read config_file: {}", &config_file);
                }
            };
        }

        if base_config.agent_key.trim() == "" || base_config.account_url.trim() == "" {
            panic!("agent-key or account-url not given.");
        }

        base_config
    }

    fn line_value(&self, line: &str) -> String {
        let value = line
            .trim()
            .split(":")
            .map(|x| x.trim().to_string())
            .collect::<Vec<String>>();

        if value.len() != 2 {
            return "".to_string();
        }

        return value[1].clone();
    }

    pub fn apply_config_file(&mut self, filename: &str) -> Result<(), String> {
        let file =
            File::open(filename).or_else(|_| Err("could not open config file".to_string()))?;
        let mut buf_reader = BufReader::new(file);

        let mut content = String::new();

        buf_reader
            .read_to_string(&mut content)
            .or_else(|_| Err("could not read config file".to_string()))?;

        for line in content.split("\n") {
            if line.trim().starts_with("#") || line.trim().starts_with("[") {
                continue;
            }

            if line.trim().starts_with("agent_key") {
                self.agent_key = self.line_value(&line);
                continue;
            }

            if line.trim().starts_with("sd_account") {
                self.account_url = self.line_value(&line);
                continue;
            }
        }

        Ok(())
    }
}

pub struct ServerDensityAggregator {
    config: ServerDensityConfig,
    http_client: Client,
    api_postback_uri: String,
}

impl ServerDensityAggregator {
    pub fn new(config: ServerDensityConfig) -> ServerDensityAggregator {
        ServerDensityAggregator {
            config: config.clone(),
            http_client: Client::new(),
            api_postback_uri: format!(
                "{}/alerts/postbacks?token={}",
                &config.serverdensity_endpoint, &config.token
            ),
        }
    }

    pub fn run(&self, receiver: Receiver<InboundMetric>) {
        let regex = Regex::new(r"[^0-9a-zA-ZäöüÄÖÜß\-\(\)\._]*").expect("failed to compile regex");

        let mut metricmap = HashMap::new();
        let mut sys_time = SystemTime::now();

        let handler_sum = SumHandler::new();
        let mut handler_avg = AverageHandler::new();
        let handler_peak = PeakHandler::new();
        let handler_min = MinHandler::new();

        loop {
            thread::sleep(Duration::from_millis(30));

            loop {
                let mut i = 0;

                match receiver.try_recv() {
                    Ok(metric) => {
                        let metric_name = regex.replace_all(&metric.name, "").trim().to_string();

                        if metric_name == "" {
                            println!("got empty metric name.");
                            continue;
                        }

                        match metric.metric_type {
                            MetricType::Sum => {
                                handler_sum.handle(&metric_name, &metric, &mut metricmap);
                            }
                            MetricType::Average => {
                                handler_avg.handle(&metric_name, &metric, &mut metricmap);
                            }
                            MetricType::Peak => {
                                handler_peak.handle(&metric_name, &metric, &mut metricmap);
                            }
                            MetricType::Min => {
                                handler_min.handle(&metric_name, &metric, &mut metricmap);
                            }
                        };

                        i = i + 1;

                        if i == 50_000 {
                            println!(
                                "got a lot of messages, may more than the server can handel..."
                            );
                        }
                    }
                    Err(TryRecvError::Empty) => {
                        break;
                    }
                    Err(TryRecvError::Disconnected) => {
                        panic!("channel disconnected, should never happen.");
                    }
                };
            }

            let elapsed_time = match sys_time.elapsed() {
                Ok(t) => t,
                Err(_) => {
                    println!("seems to have trouble with the clock, should never happen.");
                    continue;
                }
            };

            if elapsed_time.as_secs() >= 10 {
                sys_time = SystemTime::now();
                handler_sum.flush(&mut metricmap);
                handler_avg.flush(&mut metricmap);
                handler_peak.flush(&mut metricmap);
                handler_min.flush(&mut metricmap);
                self.push_to_serverdensity(&mut metricmap);
            }
        }
    }

    pub fn create_plugin_map(map: &HashMap<String, i32>) -> String {
        let mut outermap: HashMap<String, HashMap<String, i32>> = HashMap::new();

        for (k, v) in map {
            let len = k.len();

            match k.find(".") {
                Some(index) if index > 0 && index + 1 != len => {
                    outermap
                        .entry(k[..index].to_string())
                        .or_insert(HashMap::new())
                        .insert(k[index + 1..].to_string(), v.clone());
                }
                _ if k.trim() != "" => {
                    outermap
                        .entry("custom".to_string())
                        .or_insert(HashMap::new())
                        .insert(k.to_string(), v.clone());
                }
                _ => {}
            };
        }

        outermap
            .iter()
            .map(|(k, v)| {
                let mut buf = String::new();
                buf.push_str("\"");
                buf.push_str(k);
                buf.push_str("\":{");

                buf.push_str(
                    &v.iter()
                        .map(|(k, v)| format!("\"{}\":\"{}\"", k, v))
                        .collect::<Vec<String>>()
                        .join(",")
                        .to_string(),
                );

                buf.push_str("}");
                buf
            })
            .collect::<Vec<String>>()
            .join(",")
            .to_string()
    }

    pub fn push_to_serverdensity(&self, metricmap: &mut HashMap<String, i32>) {
        if metricmap.len() == 0 {
            return;
        }

        let mut payload = "{\"agentKey\":\"".to_string();
        payload.push_str(&self.config.agent_key);
        payload.push_str("\",\"plugins\":{");
        payload.push_str(&Self::create_plugin_map(metricmap));
        payload.push_str("}}");

        *metricmap = HashMap::new();

        let send_data_to_backend_time = SystemTime::now();

        let data = &[
            ("payload", &payload),
            ("hash", &format!("{:x}", md5::compute(&payload))),
        ];

        println!("Data to send to ServerDensity Backend {:#?}", &data);

        let mut res = self
            .http_client
            .post(&self.api_postback_uri)
            .header("X-Forwarded-Host", self.config.account_url.clone())
            .form(data)
            .timeout(Duration::from_secs(30))
            .send();

        let send_data_to_backend_tooked_in_ms = match send_data_to_backend_time.elapsed() {
            Ok(duration) => {
                (duration.as_secs() * 1000) + (duration.subsec_nanos() as u64 / 1000000)
            }
            Err(_) => {
                println!("seems to have trouble with the clock, should never happen.");
                return;
            }
        };

        match &mut res {
            &mut Ok(ref mut r) => {
                let mut content = String::new();
                match r.read_to_string(&mut content) {
                    Ok(_) => {
                        println!("submitted to serverdensity, tooked {}ms \n--- metrics --- \n{:#?} \n--- Request ---\n{:#?} \n\n{} \n----\n", &send_data_to_backend_tooked_in_ms, data, r, &content);
                    }
                    Err(_) => {
                        println!("submitted to serverdentity, status: {:?}, but could not read response.", r);
                    }
                }
            }
            &mut Err(ref mut e) => {
                println!("failed to send to serverdensity, status {:?}", e.status());
                println!("error: {:?}", e);
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use crate::serverdensity::aggregator::ServerDensityAggregator;
    use ::std::collections::HashMap;

    #[test]
    fn it_works() {
        let mut m = HashMap::new();
        m.insert("foo".to_string(), 2);
        m.insert("foo.".to_string(), 3);
        m.insert(".foo.bar.barr".to_string(), 4);

        m.insert("foo.bar".to_string(), 5);
        m.insert("foo.bar.baz".to_string(), 6);

        let out = ServerDensityAggregator::create_plugin_map(&m);

        println!("{}\n", &format!("{}", out));

        let mut m = HashMap::new();
        m.insert("foo".to_string(), 2);
        let out = ServerDensityAggregator::create_plugin_map(&m);
        println!("{}\n", &format!("{}", out));

        let mut m = HashMap::new();
        m.insert("foo.bar".to_string(), 2);
        let out = ServerDensityAggregator::create_plugin_map(&m);
        println!("{}\n", &format!("{}", out));

        let mut m = HashMap::new();
        let out = ServerDensityAggregator::create_plugin_map(&m);
        println!("{}\n", &format!("{}", out));

        /*
        the assert doesnt work because the map is not ordered.

        assert_eq!(
            r#""foo":{"bar":"5","bar.baz":"6"},"custom":{".foo.bar.barr":"4","foo":"2","foo.":"3"}"#,
            &format!("{}", out)
        );
        */
    }
}