// This file is part of Edgehog.
//
// Copyright 2023-2024 SECO Mind Srl
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// SPDX-License-Identifier: Apache-2.0

//! Container properties sent from the device to Astarte.

use astarte_device_sdk::AstarteType;
use async_trait::async_trait;
use tracing::error;
use uuid::Uuid;

cfg_if::cfg_if! {
    if #[cfg(test)] {
        pub use astarte_device_sdk_mock::Client;
    } else {
        pub use astarte_device_sdk::Client;
    }
}

pub(crate) mod container;
pub(crate) mod deployment;
pub(crate) mod image;
pub(crate) mod network;
pub(crate) mod volume;

#[async_trait]
pub(crate) trait AvailableProp {
    type Data: Into<AstarteType> + Send + 'static;

    fn interface() -> &'static str;

    fn field() -> &'static str;

    fn id(&self) -> &Uuid;

    async fn send<D>(&self, device: &D, data: Self::Data)
    where
        D: Client + Sync + 'static,
    {
        self.send_field(device, Self::field(), data).await;
    }

    async fn send_field<D, T>(&self, device: &D, field: &str, data: T)
    where
        D: Client + Sync + 'static,
        T: Into<AstarteType> + Send + 'static,
    {
        let interface = Self::interface();
        let endpoint = format!("/{}/{}", self.id(), field);

        let res = device.send(interface, &endpoint, data).await;

        if let Err(err) = res {
            error!(
                error = format!("{:#}", eyre::Report::new(err)),
                "couldn't send data for {interface}{endpoint}"
            );
        }
    }

    async fn unset<D>(&self, device: &D)
    where
        D: Client + Sync + 'static,
    {
        let interface = Self::interface();
        let field = Self::field();
        let endpoint = format!("/{}/{}", self.id(), field);

        let res = device.unset(interface, &endpoint).await;

        if let Err(err) = res {
            error!(
                error = format!("{:#}", eyre::Report::new(err)),
                "couldn't send data for {interface}{endpoint}"
            );
        }
    }
}
