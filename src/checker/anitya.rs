use std::collections::HashMap;

use super::UpdateChecker;
use crate::must_have;
use anyhow::{anyhow, Result};
use reqwest::blocking::Client;
use serde::Deserialize;

const API_ENDPOINT: &str = "https://release-monitoring.org/api/project/";

#[derive(Deserialize)]
struct AnityaData {
    id: usize,
    stable_versions: Vec<String>,
}

pub(crate) struct AnityaChecker {
    id: usize,
}

impl UpdateChecker for AnityaChecker {
    fn new(config: &HashMap<String, String>) -> Result<Self> {
        let id = must_have!(config, "id", "Anitya project ID")?.parse::<usize>()?;

        Ok(AnityaChecker { id })
    }

    fn check(&self, client: &Client) -> Result<String> {
        let resp = client
            .get(&format!("{}{}/", API_ENDPOINT, self.id))
            .send()?;
        let payload: AnityaData = resp.json()?;
        if payload.id != self.id {
            return Err(anyhow!(
                "The unthinkable happened: requested ID and received ID mismatch."
            ));
        }
        if payload.stable_versions.len() < 1 {
            return Err(anyhow!("Anitya didn't return any stable versions!"));
        }

        Ok(payload.stable_versions[0].clone())
    }
}

#[test]
fn test_check_anitya() {
    let mut options = HashMap::new();
    options.insert("id".to_string(), "1832".to_string()); // lmms
    let client = Client::new();
    let checker = AnityaChecker::new(&options).unwrap();
    checker.check(&client).unwrap();
}
