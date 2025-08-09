use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct ConfigReplica {
  pub address: String,
}

#[derive(Deserialize, Debug)]
pub struct Config {
  pub version: u8,
  #[serde(rename = "replica")]
  pub replicas: Vec<ConfigReplica>,
}
