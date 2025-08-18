use std::collections::HashMap;

use vr_proxy::Proxy;

pub struct State {
  pub proxy: Proxy,
  pub state: HashMap<String, String>,
}

impl State {
  pub fn new(proxy: Proxy) -> Self {
    Self {
      proxy,
      state: HashMap::new(),
    }
  }
}
