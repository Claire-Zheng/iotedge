// Copyright (c) Microsoft. All rights reserved.

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub enum Health {
    Healthy,
    Degraded,
    Poor,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct HealthStatus {
    health: Health,
    status: Status,
}

impl HealthStatus {
    pub fn new(health: Health, status: Status) -> Self {
        HealthStatus { health, status }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Status {
    iotedged: bool,
    edge_agent: bool,
    edge_hub: bool,
    other_modules: bool,
}

impl Status {
    pub fn new() -> Self {
        Status {
            iotedged: false,
            edge_agent: false,
            edge_hub: false,
            other_modules: false,
        }
    }

    pub fn set_iotedged(&mut self) {
        self.iotedged = true;
    }

    pub fn set_edge_agent(&mut self, val: bool) {
        self.edge_agent = val;
    }

    pub fn set_edge_hub(&mut self, val: bool) {
        self.edge_hub = val;
    }

    pub fn set_other_modules(&mut self, val: bool) {
        self.other_modules = val;
    }

    pub fn return_health(&self) -> Health {
        if self.iotedged && self.edge_agent && self.edge_hub {
            if self.other_modules {
                Health::Healthy
            } else {
                Health::Degraded
            }
        } else {
            Health::Poor
        }
    }
}
