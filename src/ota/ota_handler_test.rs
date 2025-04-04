/*
 * This file is part of Edgehog.
 *
 * Copyright 2023 SECO Mind Srl
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *   http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 *
 * SPDX-License-Identifier: Apache-2.0
 */

use std::time::Duration;

use crate::controller::actor::Actor;
use crate::data::tests::MockPubSub;
use crate::ota::event::{OtaOperation, OtaRequest};
use futures::StreamExt;
use httpmock::prelude::*;
use mockall::Sequence;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::error::DeviceManagerError;
use crate::ota::ota_handler::{OtaEvent, OtaHandler, OtaMessage};
use crate::ota::rauc::BundleInfo;
use crate::ota::{DeployProgress, DeployStatus, MockSystemUpdate, OtaError, ProgressStream};
use crate::ota::{Ota, OtaId, OtaStatus, PersistentState};
use crate::repository::MockStateRepository;

use super::ota_handler::OtaPublisher;

pub(crate) fn deploy_status_stream<I>(iter: I) -> Result<ProgressStream, DeviceManagerError>
where
    I: IntoIterator<Item = DeployStatus>,
{
    Ok(futures::stream::iter(iter.into_iter().map(Ok).collect::<Vec<_>>()).boxed())
}

impl OtaPublisher<MockPubSub> {
    fn mock_new(
        client: MockPubSub,
        publisher_rx: mpsc::Receiver<OtaStatus>,
    ) -> JoinHandle<stable_eyre::Result<()>> {
        let publisher = Self::new(client);

        tokio::spawn(publisher.spawn(publisher_rx))
    }
}

impl OtaHandler {
    fn mock_new_with_path(
        system_update: MockSystemUpdate,
        state_repository: MockStateRepository<PersistentState>,
        prefix: &str,
        publisher_tx: mpsc::Sender<OtaStatus>,
    ) -> (Self, tempdir::TempDir) {
        let (ota, dir) =
            Ota::mock_new_with_path(system_update, state_repository, prefix, publisher_tx);

        let handler = Self::mock_new_with_ota(ota);

        (handler, dir)
    }

    fn mock_new_with_ota(ota: Ota<MockSystemUpdate, MockStateRepository<PersistentState>>) -> Self {
        let (ota_tx, ota_rx) = mpsc::channel(8);

        let flag = ota.flag.clone();
        let publisher_tx = ota.publisher_tx.clone();

        tokio::spawn(async move {
            ota.spawn(ota_rx).await.unwrap();
        });

        Self {
            ota_tx,
            publisher_tx,
            flag,
            current: None,
        }
    }
}

#[tokio::test]
async fn handle_ota_event_bundle_not_compatible() {
    let uuid = Uuid::new_v4();

    let bundle_info = "rauc-demo-x86";
    let system_info = "rauc-demo-arm";

    let mut state_mock = MockStateRepository::<PersistentState>::new();
    let mut seq = Sequence::new();

    state_mock
        .expect_exists()
        .times(2)
        .in_sequence(&mut seq)
        .returning(|| false);

    state_mock
        .expect_write()
        .withf(move |p| p.uuid == uuid && p.slot == "B")
        .once()
        .in_sequence(&mut seq)
        .returning(|_| Ok(()));

    state_mock
        .expect_clear()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok(()));

    let mut system_update = MockSystemUpdate::new();
    let mut seq = Sequence::new();

    system_update
        .expect_info()
        .once()
        .in_sequence(&mut seq)
        .returning(|_: &str| {
            Ok(BundleInfo {
                compatible: bundle_info.to_string(),
                version: "1".to_string(),
            })
        });

    system_update
        .expect_compatible()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok(system_info.to_string()));

    system_update
        .expect_boot_slot()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok("B".to_owned()));

    system_update
        .expect_install_bundle()
        .once()
        .in_sequence(&mut seq)
        .returning(|_| Err(DeviceManagerError::Fatal("install fail".to_string())));

    let binary_content = b"\x80\x02\x03";
    let binary_size = binary_content.len();

    let server = MockServer::start_async().await;
    let ota_url = server.url("/ota.bin");
    let mock_ota_file_request = server
        .mock_async(|when, then| {
            when.method(GET).path("/ota.bin");
            then.status(200)
                .header("content-Length", binary_size.to_string())
                .body(binary_content);
        })
        .await;

    let ota_req_map = OtaRequest {
        url: ota_url.clone(),
        operation: OtaOperation::Update,
        uuid: uuid.into(),
    };

    let (publisher_tx, mut publisher_rx) = mpsc::channel(8);

    let (mut ota_handler, _dir) =
        OtaHandler::mock_new_with_path(system_update, state_mock, "fail_deployed", publisher_tx);
    ota_handler.handle_event(ota_req_map).await.unwrap();

    let ota_id = OtaId { uuid, url: ota_url };
    let exp = [
        OtaStatus::Idle,
        OtaStatus::Init(ota_id.clone()),
        OtaStatus::Acknowledged(ota_id.clone()),
        OtaStatus::Downloading(ota_id.clone(), 0),
        OtaStatus::Downloading(ota_id.clone(), 100),
        OtaStatus::Failure(
            OtaError::InvalidBaseImage(format!(
                "bundle {bundle_info} is not compatible with system {system_info}"
            )),
            Some(ota_id.clone()),
        ),
        OtaStatus::Idle,
    ];

    for status in exp {
        let val = tokio::time::timeout(Duration::from_secs(2), publisher_rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(val, status);
    }

    mock_ota_file_request.assert_async().await;
}

#[tokio::test]
async fn handle_ota_event_bundle_install_completed_fail() {
    let uuid = Uuid::new_v4();

    let mut state_mock = MockStateRepository::<PersistentState>::new();
    let mut seq = Sequence::new();

    state_mock
        .expect_exists()
        .times(2)
        .in_sequence(&mut seq)
        .returning(|| false);

    state_mock
        .expect_write()
        .once()
        .withf(move |p| p.uuid == uuid && p.slot == "A")
        .in_sequence(&mut seq)
        .returning(|_| Ok(()));

    state_mock
        .expect_exists()
        .once()
        .in_sequence(&mut seq)
        .returning(|| true);

    state_mock
        .expect_clear()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok(()));

    let mut system_update = MockSystemUpdate::new();
    let mut seq = Sequence::new();

    system_update
        .expect_info()
        .once()
        .in_sequence(&mut seq)
        .returning(|_: &str| {
            Ok(BundleInfo {
                compatible: "rauc-demo-x86".to_string(),
                version: "1".to_string(),
            })
        });

    system_update
        .expect_compatible()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok("rauc-demo-x86".to_string()));

    system_update
        .expect_boot_slot()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok("A".to_owned()));

    system_update
        .expect_install_bundle()
        .once()
        .in_sequence(&mut seq)
        .returning(|_: &str| Ok(()));

    system_update
        .expect_operation()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok("".to_string()));

    system_update
        .expect_receive_completed()
        .once()
        .in_sequence(&mut seq)
        .returning(|| deploy_status_stream([DeployStatus::Completed { signal: -1 }]));

    system_update
        .expect_last_error()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok("Unable to deploy image".to_string()));

    let binary_content = b"\x80\x02\x03";
    let binary_size = binary_content.len();

    let server = MockServer::start_async().await;
    let ota_url = server.url("/ota.bin");
    let mock_ota_file_request = server
        .mock_async(|when, then| {
            when.method(GET).path("/ota.bin");
            then.status(200)
                .header("content-Length", binary_size.to_string())
                .body(binary_content);
        })
        .await;

    let req = OtaRequest {
        operation: OtaOperation::Update,
        url: ota_url.clone(),
        uuid: uuid.into(),
    };

    let (publisher_tx, mut publisher_rx) = mpsc::channel(8);

    let (mut ota_handler, _dir) =
        OtaHandler::mock_new_with_path(system_update, state_mock, "completed_fail", publisher_tx);
    ota_handler.handle_event(req).await.unwrap();

    let ota_id = OtaId { uuid, url: ota_url };
    let exp = [
        OtaStatus::Idle,
        OtaStatus::Init(ota_id.clone()),
        OtaStatus::Acknowledged(ota_id.clone()),
        OtaStatus::Downloading(ota_id.clone(), 0),
        OtaStatus::Downloading(ota_id.clone(), 100),
        OtaStatus::Deploying(
            ota_id.clone(),
            DeployProgress {
                percentage: 0,
                message: String::new(),
            },
        ),
        OtaStatus::Failure(
            OtaError::InvalidBaseImage("Update failed with signal -1".to_string()),
            Some(ota_id.clone()),
        ),
        OtaStatus::Idle,
    ];

    for status in exp {
        let val = tokio::time::timeout(Duration::from_secs(2), publisher_rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(val, status);
    }

    mock_ota_file_request.assert_async().await;
}

#[tokio::test]
async fn ota_event_fail_deployed() {
    let uuid = Uuid::new_v4();

    let mut state_mock = MockStateRepository::<PersistentState>::new();
    let mut seq = Sequence::new();

    state_mock
        .expect_exists()
        .times(2)
        .in_sequence(&mut seq)
        .returning(|| false);

    state_mock
        .expect_write()
        .withf(move |p| p.uuid == uuid && p.slot == "B")
        .once()
        .in_sequence(&mut seq)
        .returning(|_| Ok(()));

    state_mock
        .expect_clear()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok(()));

    let mut system_update = MockSystemUpdate::new();
    let mut seq = Sequence::new();

    system_update
        .expect_info()
        .once()
        .in_sequence(&mut seq)
        .returning(|_: &str| {
            Ok(BundleInfo {
                compatible: "rauc-demo-x86".to_string(),
                version: "1".to_string(),
            })
        });

    system_update
        .expect_compatible()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok("rauc-demo-x86".to_string()));

    system_update
        .expect_boot_slot()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok("B".to_owned()));

    system_update
        .expect_install_bundle()
        .once()
        .in_sequence(&mut seq)
        .returning(|_| Err(DeviceManagerError::Fatal("install fail".to_string())));

    let binary_content = b"\x80\x02\x03";
    let binary_size = binary_content.len();

    let server = MockServer::start_async().await;
    let ota_url = server.url("/ota.bin");
    let mock_ota_file_request = server
        .mock_async(|when, then| {
            when.method(GET).path("/ota.bin");
            then.status(200)
                .header("content-Length", binary_size.to_string())
                .body(binary_content);
        })
        .await;

    let ota_req_map = OtaRequest {
        url: ota_url.clone(),
        operation: OtaOperation::Update,
        uuid: uuid.into(),
    };

    let (publisher_tx, mut publisher_rx) = mpsc::channel(8);

    let (mut ota_handler, _dir) =
        OtaHandler::mock_new_with_path(system_update, state_mock, "fail_deployed", publisher_tx);
    ota_handler.handle_event(ota_req_map).await.unwrap();

    let ota_id = OtaId { uuid, url: ota_url };
    let exp = [
        OtaStatus::Idle,
        OtaStatus::Init(ota_id.clone()),
        OtaStatus::Acknowledged(ota_id.clone()),
        OtaStatus::Downloading(ota_id.clone(), 0),
        OtaStatus::Downloading(ota_id.clone(), 100),
        OtaStatus::Deploying(
            ota_id.clone(),
            DeployProgress {
                percentage: 0,
                message: String::new(),
            },
        ),
        OtaStatus::Failure(
            OtaError::InvalidBaseImage("Unable to install ota image".to_string()),
            Some(ota_id.clone()),
        ),
        OtaStatus::Idle,
    ];

    for status in exp {
        let val = tokio::time::timeout(Duration::from_secs(2), publisher_rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(val, status);
    }

    mock_ota_file_request.assert_async().await;
}

#[tokio::test]
async fn ota_event_update_success() {
    let uuid = Uuid::new_v4();

    let mut state_mock = MockStateRepository::<PersistentState>::new();
    let mut seq = Sequence::new();

    state_mock
        .expect_exists()
        .times(2)
        .in_sequence(&mut seq)
        .returning(|| false);

    state_mock
        .expect_write()
        .once()
        .withf(move |p| p.uuid == uuid && p.slot == "A")
        .in_sequence(&mut seq)
        .returning(|_| Ok(()));

    state_mock
        .expect_exists()
        .once()
        .in_sequence(&mut seq)
        .returning(|| true);

    state_mock
        .expect_read()
        .once()
        .in_sequence(&mut seq)
        .returning(move || {
            Ok(PersistentState {
                uuid,
                slot: "A".to_owned(),
            })
        });

    state_mock
        .expect_clear()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok(()));

    let mut system_update = MockSystemUpdate::new();
    let mut seq = Sequence::new();

    system_update
        .expect_info()
        .once()
        .in_sequence(&mut seq)
        .returning(|_: &str| {
            Ok(BundleInfo {
                compatible: "rauc-demo-x86".to_string(),
                version: "1".to_string(),
            })
        });

    system_update
        .expect_compatible()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok("rauc-demo-x86".to_string()));

    system_update
        .expect_boot_slot()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok("A".to_owned()));

    system_update
        .expect_install_bundle()
        .once()
        .in_sequence(&mut seq)
        .returning(|_: &str| Ok(()));

    system_update
        .expect_operation()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok("".to_string()));

    system_update
        .expect_receive_completed()
        .once()
        .in_sequence(&mut seq)
        .returning(|| deploy_status_stream([DeployStatus::Completed { signal: 0 }]));

    system_update
        .expect_boot_slot()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok("B".to_owned()));

    system_update
        .expect_get_primary()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok("rootfs.0".to_owned()));

    system_update
        .expect_mark()
        .once()
        .in_sequence(&mut seq)
        .returning(|_: &str, _: &str| {
            Ok((
                "rootfs.0".to_owned(),
                "marked slot rootfs.0 as good".to_owned(),
            ))
        });

    let binary_content = b"\x80\x02\x03";
    let binary_size = binary_content.len();

    let server = MockServer::start_async().await;
    let ota_url = server.url("/ota.bin");
    let mock_ota_file_request = server
        .mock_async(|when, then| {
            when.method(GET).path("/ota.bin");
            then.status(200)
                .header("content-Length", binary_size.to_string())
                .body(binary_content);
        })
        .await;

    let req = OtaRequest {
        operation: OtaOperation::Update,
        url: ota_url.clone(),
        uuid: uuid.into(),
    };

    let (publisher_tx, mut publisher_rx) = mpsc::channel(8);

    let (mut ota_handler, _dir) =
        OtaHandler::mock_new_with_path(system_update, state_mock, "success", publisher_tx);
    ota_handler.handle_event(req).await.unwrap();

    let ota_id = OtaId { uuid, url: ota_url };
    let exp = [
        OtaStatus::Idle,
        OtaStatus::Init(ota_id.clone()),
        OtaStatus::Acknowledged(ota_id.clone()),
        OtaStatus::Downloading(ota_id.clone(), 0),
        OtaStatus::Downloading(ota_id.clone(), 100),
        OtaStatus::Deploying(
            ota_id.clone(),
            DeployProgress {
                percentage: 0,
                message: String::new(),
            },
        ),
        OtaStatus::Deployed(ota_id.clone()),
        OtaStatus::Rebooting(ota_id.clone()),
        OtaStatus::Rebooted,
        OtaStatus::Success(OtaId {
            uuid,
            url: String::new(),
        }),
        OtaStatus::Idle,
    ];

    for status in exp {
        let val = tokio::time::timeout(Duration::from_secs(2), publisher_rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(val, status);
    }

    mock_ota_file_request.assert_async().await;
}

#[tokio::test]
async fn ota_event_update_already_in_progress_same_uuid() {
    let uuid = Uuid::new_v4();

    let mut state_mock = MockStateRepository::<PersistentState>::new();
    let mut seq = Sequence::new();

    state_mock
        .expect_exists()
        .times(2)
        .in_sequence(&mut seq)
        .returning(|| false);

    let system_update = MockSystemUpdate::new();

    let ota_url = "http://localhost".to_string();

    let req = OtaRequest {
        operation: OtaOperation::Update,
        url: ota_url.clone(),
        uuid: uuid.into(),
    };

    let (publisher_tx, mut publisher_rx) = mpsc::channel(1);

    let ota = Ota::mock_new(system_update, state_mock, publisher_tx);
    let mut ota_handler = OtaHandler::mock_new_with_ota(ota);

    ota_handler.handle_event(req.clone()).await.unwrap();
    assert!(ota_handler.current.is_some());
    assert!(
        ota_handler
            .check_update_already_in_progress(&OtaId {
                uuid,
                url: ota_url.clone()
            })
            .await
    );
    ota_handler.handle_event(req.clone()).await.unwrap();

    // Expect no error and the update is progressing
    let ota_id = OtaId { uuid, url: ota_url };
    let exp = [
        OtaStatus::Idle,
        OtaStatus::Init(ota_id.clone()),
        OtaStatus::Acknowledged(ota_id.clone()),
        OtaStatus::Downloading(ota_id.clone(), 0),
    ];

    for status in exp {
        let val = tokio::time::timeout(Duration::from_secs(2), publisher_rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(val, status);
    }
}

#[tokio::test]
async fn ota_event_update_already_in_progress_different_uuid() {
    let uuid_1 = Uuid::new_v4();
    let uuid_2 = Uuid::new_v4();

    let mut state_mock = MockStateRepository::<PersistentState>::new();
    let mut seq = Sequence::new();
    state_mock
        .expect_exists()
        .times(2)
        .in_sequence(&mut seq)
        .returning(|| false);

    let system_update = MockSystemUpdate::new();

    let ota_url = "http://localhost".to_string();

    let req1 = OtaRequest {
        operation: OtaOperation::Update,
        url: ota_url.clone(),
        uuid: uuid_1.into(),
    };

    let req2 = OtaRequest {
        operation: OtaOperation::Update,
        url: ota_url.clone(),
        uuid: uuid_2.into(),
    };

    let (publisher_tx, mut publisher_rx) = mpsc::channel(2);

    let ota = Ota::mock_new(system_update, state_mock, publisher_tx);
    let mut ota_handler = OtaHandler::mock_new_with_ota(ota);

    ota_handler.handle_event(req1.clone()).await.unwrap();
    assert!(ota_handler.current.is_some());
    assert!(
        ota_handler
            .check_update_already_in_progress(&OtaId {
                uuid: uuid_2,
                url: ota_url.clone()
            })
            .await
    );
    ota_handler.handle_event(req2.clone()).await.unwrap();

    // Expect no error and the update is progressing
    let ota_id = OtaId {
        uuid: uuid_1,
        url: ota_url.clone(),
    };
    let exp = [
        OtaStatus::Idle,
        OtaStatus::Init(ota_id.clone()),
        OtaStatus::Acknowledged(ota_id.clone()),
    ];

    let failure = OtaStatus::Failure(
        OtaError::UpdateAlreadyInProgress,
        Some(OtaId {
            uuid: uuid_2,
            url: ota_url,
        }),
    );

    let mut count = 0;
    for status in exp {
        'failed: loop {
            let val = tokio::time::timeout(Duration::from_secs(2), publisher_rx.recv())
                .await
                .unwrap()
                .unwrap();
            // At least one failure
            if val == failure {
                count += 1;
                continue;
            }

            assert_eq!(val, status);

            break 'failed;
        }
    }

    // 1 real, One for the checked update
    assert_eq!(count, 2);
}

#[tokio::test]
async fn ota_event_canceled() {
    let uuid = Uuid::new_v4();
    let cancel_token = CancellationToken::new();

    let mut client = MockPubSub::new();

    client
        .expect_send_object()
        .withf(move |_: &str, _: &str, ota_event: &OtaEvent| {
            ota_event.status.eq("Failure")
                && ota_event.statusCode.eq("Canceled")
                && ota_event.statusProgress == 0
                && ota_event.requestUUID == uuid.to_string()
        })
        .once()
        .returning(|_: &str, _: &str, _: OtaEvent| Ok(()));

    let (publisher_tx, publisher_rx) = mpsc::channel(8);
    OtaPublisher::mock_new(client, publisher_rx);

    let state_repository = MockStateRepository::<PersistentState>::new();
    let system_update = MockSystemUpdate::new();

    let mut ota = Ota::mock_new(system_update, state_repository, publisher_tx);
    ota.ota_status = OtaStatus::Acknowledged(OtaId {
        uuid,
        url: "".to_string(),
    });

    let mut ota_handler = OtaHandler::mock_new_with_ota(ota);
    ota_handler.current = Some(OtaMessage {
        ota_id: OtaId {
            uuid,
            url: String::new(),
        },
        cancel: cancel_token.clone(),
    });

    let ota_req_map = OtaRequest {
        operation: OtaOperation::Cancel,
        url: String::new(),
        uuid: uuid.into(),
    };

    let res = ota_handler.handle_event(ota_req_map).await;

    assert!(res.is_ok(), "ota cancel failed with: {}", res.unwrap_err());

    assert!(cancel_token.is_cancelled());
}

#[tokio::test]
async fn ota_event_success_after_canceled_event() {
    let uuid_1 = Uuid::new_v4();
    let uuid_2 = Uuid::new_v4();
    let slot = "A";

    let mut state_mock = MockStateRepository::<PersistentState>::new();
    let mut seq = Sequence::new();

    // check first reboot
    state_mock
        .expect_exists()
        .once()
        .in_sequence(&mut seq)
        .returning(|| false);
    // clear for no pending reboot
    state_mock
        .expect_exists()
        .once()
        .in_sequence(&mut seq)
        .returning(|| false);
    // clear for cancel
    state_mock
        .expect_exists()
        .once()
        .in_sequence(&mut seq)
        .returning(|| false);
    // Write success ota
    state_mock
        .expect_write()
        .once()
        .in_sequence(&mut seq)
        .withf(move |ps| ps.uuid == uuid_2 && ps.slot == "B")
        .returning(|_| Ok(()));
    // reboot successful OTA
    state_mock
        .expect_exists()
        .once()
        .in_sequence(&mut seq)
        .returning(|| true);
    state_mock
        .expect_read()
        .times(1)
        .in_sequence(&mut seq)
        .returning(move || {
            Ok(PersistentState {
                uuid: uuid_2,
                slot: slot.to_owned(),
            })
        });
    // clear
    state_mock
        .expect_exists()
        .once()
        .in_sequence(&mut seq)
        .returning(|| true);
    state_mock
        .expect_clear()
        .once()
        .in_sequence(&mut seq)
        .returning(|| Ok(()));

    let mut system_update = MockSystemUpdate::new();
    system_update.expect_info().returning(|_: &str| {
        Ok(BundleInfo {
            compatible: "rauc-demo-x86".to_string(),
            version: "1".to_string(),
        })
    });

    system_update
        .expect_compatible()
        .returning(|| Ok("rauc-demo-x86".to_string()));

    system_update
        .expect_boot_slot()
        .returning(|| Ok("B".to_owned()));
    system_update
        .expect_install_bundle()
        .returning(|_: &str| Ok(()));
    system_update
        .expect_operation()
        .returning(|| Ok("".to_string()));
    system_update
        .expect_receive_completed()
        .returning(|| deploy_status_stream([DeployStatus::Completed { signal: 0 }]));
    system_update
        .expect_get_primary()
        .returning(|| Ok("rootfs.0".to_owned()));
    system_update.expect_mark().returning(|_: &str, _: &str| {
        Ok((
            "rootfs.0".to_owned(),
            "marked slot rootfs.0 as good".to_owned(),
        ))
    });

    let binary_content = b"\x80\x02\x03";
    let binary_size = binary_content.len();

    let server = MockServer::start_async().await;
    let file_req = server
        .mock_async(|when, then| {
            when.method(GET).path("/ota.bin");
            then.status(200)
                .header("Content-Length", binary_size.to_string())
                .body(binary_content);
        })
        .await;
    let ota_url = server.url("/ota.bin");

    let ota_update = OtaRequest {
        operation: OtaOperation::Update,
        uuid: uuid_1.into(),
        url: ota_url.clone(),
    };

    let (publisher_tx, mut publisher_rx) = mpsc::channel(1);

    let (mut ota_handler, _dir) =
        OtaHandler::mock_new_with_path(system_update, state_mock, "after_cancelled", publisher_tx);

    // Start the update but handle the flow, so we can cancel it.
    ota_handler
        .handle_event(ota_update)
        .await
        .expect("failed to start ota");

    let to_cancel_id = OtaId {
        uuid: uuid_1,
        url: ota_url.clone(),
    };
    let exp = [
        OtaStatus::Idle,
        OtaStatus::Init(to_cancel_id.clone()),
        OtaStatus::Acknowledged(to_cancel_id.clone()),
    ];

    for status in exp {
        let val = tokio::time::timeout(Duration::from_secs(2), publisher_rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(val, status);
    }

    // We send the cancel event in another thread and wait for the response
    let ota_cancel = OtaRequest {
        operation: OtaOperation::Cancel,
        url: ota_url.clone(),
        uuid: uuid_1.into(),
    };
    ota_handler.handle_event(ota_cancel).await.unwrap();

    let exp = [
        OtaStatus::Downloading(to_cancel_id.clone(), 0),
        OtaStatus::Failure(OtaError::Canceled, Some(to_cancel_id.clone())),
        OtaStatus::Idle,
    ];

    for status in exp {
        let val = tokio::time::timeout(Duration::from_secs(2), publisher_rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(val, status);
    }

    let ota_update = OtaRequest {
        operation: OtaOperation::Update,
        url: ota_url.clone(),
        uuid: uuid_2.into(),
    };

    ota_handler.handle_event(ota_update.clone()).await.unwrap();

    let ota_id = OtaId {
        uuid: uuid_2,
        url: ota_url,
    };
    let exp = [
        OtaStatus::Init(ota_id.clone()),
        OtaStatus::Acknowledged(ota_id.clone()),
        OtaStatus::Downloading(ota_id.clone(), 0),
        OtaStatus::Downloading(ota_id.clone(), 100),
        OtaStatus::Deploying(
            ota_id.clone(),
            DeployProgress {
                percentage: 0,
                message: "".to_string(),
            },
        ),
        OtaStatus::Deployed(ota_id.clone()),
        OtaStatus::Rebooting(ota_id.clone()),
        OtaStatus::Rebooted,
        OtaStatus::Success(OtaId {
            uuid: uuid_2,
            url: "".to_string(),
        }),
        OtaStatus::Idle,
    ];

    for status in exp {
        let val = tokio::time::timeout(Duration::from_secs(2), publisher_rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(val, status);
    }

    // One for the cancelled and one for the successful
    file_req.assert_async().await;
}

/// Try to cancel an OTA without a cancel token (OTA finished, same uuid)
#[tokio::test]
async fn ota_event_not_canceled() {
    let uuid = Uuid::new_v4();

    let state_mock = MockStateRepository::<PersistentState>::new();
    let system_update = MockSystemUpdate::new();

    let ota_req_map = OtaRequest {
        operation: OtaOperation::Cancel,
        url: String::new(),
        uuid: uuid.into(),
    };

    let mut client = MockPubSub::new();

    client
        .expect_send_object()
        .withf(move |_: &str, _: &str, ota_event: &OtaEvent| {
            ota_event.status.eq("Failure")
                && ota_event.statusCode.eq("InternalError")
                && ota_event.statusProgress == 0
                && ota_event.requestUUID == uuid.to_string()
                && ota_event.message.eq("Unable to cancel OTA request")
        })
        .once()
        .returning(|_: &str, _: &str, _: OtaEvent| Ok(()));

    let (publisher_tx, publisher_rx) = mpsc::channel(8);
    OtaPublisher::mock_new(client, publisher_rx);

    let (mut ota, _dir) =
        Ota::mock_new_with_path(system_update, state_mock, "not_cancelled", publisher_tx);
    ota.ota_status = OtaStatus::Success(OtaId {
        uuid,
        url: "".to_string(),
    });
    let mut ota_handler = OtaHandler::mock_new_with_ota(ota);

    let result = ota_handler.handle_event(ota_req_map).await;

    assert!(
        result.is_ok(),
        "expected ota event to be Ok, but got: {}",
        result.unwrap_err()
    );
}

/// Try to cancel an OTA without a cancel token (no OTA started)
#[tokio::test]
async fn ota_event_not_canceled_empty() {
    let uuid = Uuid::new_v4();

    let state_mock = MockStateRepository::<PersistentState>::new();
    let system_update = MockSystemUpdate::new();

    let ota_req_map = OtaRequest {
        operation: OtaOperation::Cancel,
        url: String::new(),
        uuid: uuid.into(),
    };

    let mut client = MockPubSub::new();

    client
        .expect_send_object()
        .withf(move |_: &str, _: &str, ota_event: &OtaEvent| {
            ota_event.status.eq("Failure")
                && ota_event.statusCode.eq("InternalError")
                && ota_event.statusProgress == 0
                && ota_event.requestUUID == uuid.to_string()
                && ota_event
                    .message
                    .eq("Unable to cancel OTA request, internal request is empty")
        })
        .once()
        .returning(|_: &str, _: &str, _: OtaEvent| Ok(()));

    let (publisher_tx, publisher_rx) = mpsc::channel(8);
    OtaPublisher::mock_new(client, publisher_rx);

    let (mut ota_handler, _dir) = OtaHandler::mock_new_with_path(
        system_update,
        state_mock,
        "not_cancelled_empty",
        publisher_tx,
    );

    let result = ota_handler.handle_event(ota_req_map).await;

    assert!(
        result.is_ok(),
        "expected ota event to be Ok, but got: {}",
        result.unwrap_err()
    );
}

#[tokio::test]
async fn ota_event_not_canceled_different_uuid() {
    let uuid = Uuid::new_v4();
    let uuid_2 = Uuid::new_v4();

    let state_mock = MockStateRepository::<PersistentState>::new();
    let system_update = MockSystemUpdate::new();

    let ota_req_map = OtaRequest {
        operation: OtaOperation::Cancel,
        url: String::new(),
        uuid: uuid.into(),
    };

    let mut client = MockPubSub::new();

    client
        .expect_send_object()
        .withf(move |_: &str, _: &str, ota_event: &OtaEvent| {
            ota_event.status.eq("Failure")
                && ota_event.statusCode.eq("InternalError")
                && ota_event.statusProgress == 0
                && ota_event.requestUUID == uuid.to_string()
                && ota_event
                    .message
                    .eq("Unable to cancel OTA request, they have different identifier")
        })
        .once()
        .returning(|_: &str, _: &str, _: OtaEvent| Ok(()));

    let (publisher_tx, publisher_rx) = mpsc::channel(8);
    OtaPublisher::mock_new(client, publisher_rx);

    let (mut ota, _dir) = Ota::mock_new_with_path(
        system_update,
        state_mock,
        "calcelled_different_uuid",
        publisher_tx,
    );
    ota.ota_status = OtaStatus::Deployed(OtaId {
        uuid: uuid_2,
        url: "".to_string(),
    });
    let mut ota_handler = OtaHandler::mock_new_with_ota(ota);

    let result = ota_handler.handle_event(ota_req_map).await;

    assert!(
        result.is_ok(),
        "expected ota event to be Ok, but got: {}",
        result.unwrap_err()
    );
}

#[tokio::test]
async fn ensure_pending_ota_ota_is_done_fail() {
    let uuid = Uuid::new_v4();
    let slot = "A";

    let mut state_mock = MockStateRepository::<PersistentState>::new();
    state_mock.expect_exists().returning(|| true);
    state_mock.expect_read().returning(move || {
        Ok(PersistentState {
            uuid,
            slot: slot.to_owned(),
        })
    });

    state_mock.expect_clear().returning(|| Ok(()));

    let mut system_update = MockSystemUpdate::new();
    system_update
        .expect_boot_slot()
        .returning(|| Ok("A".to_owned()));

    let (publisher_tx, mut publisher_rx) = mpsc::channel(8);

    let mut ota = Ota::mock_new(system_update, state_mock, publisher_tx);
    ota.init().await.unwrap();

    let exp = [
        OtaStatus::Rebooted,
        OtaStatus::Failure(
            OtaError::SystemRollback("Unable to switch slot"),
            Some(OtaId {
                uuid,
                url: String::new(),
            }),
        ),
        OtaStatus::Idle,
    ];

    for status in exp {
        let val = tokio::time::timeout(Duration::from_secs(2), publisher_rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(val, status);
    }
}

#[tokio::test]
async fn ensure_pending_ota_is_done_ota_success() {
    let uuid = Uuid::new_v4();
    let slot = "A";
    let mut state_mock = MockStateRepository::<PersistentState>::new();
    state_mock.expect_exists().returning(|| true);
    state_mock.expect_read().returning(move || {
        Ok(PersistentState {
            uuid,
            slot: slot.to_owned(),
        })
    });
    state_mock.expect_write().returning(|_| Ok(()));
    state_mock.expect_clear().returning(|| Ok(()));

    let mut system_update = MockSystemUpdate::new();
    system_update
        .expect_boot_slot()
        .returning(|| Ok("B".to_owned()));
    system_update
        .expect_get_primary()
        .returning(|| Ok("rootfs.0".to_owned()));
    system_update.expect_mark().returning(|_: &str, _: &str| {
        Ok((
            "rootfs.0".to_owned(),
            "marked slot rootfs.0 as good".to_owned(),
        ))
    });

    let mut client = MockPubSub::new();
    let mut seq = mockall::Sequence::new();

    client
        .expect_send_object()
        .withf(move |_: &str, _: &str, ota_event: &OtaEvent| {
            ota_event.status.eq("Success")
                && ota_event.statusCode.is_empty()
                && ota_event.statusProgress == 0
                && ota_event.requestUUID == uuid.to_string()
        })
        .once()
        .returning(|_: &str, _: &str, _: OtaEvent| Ok(()))
        .in_sequence(&mut seq);

    let (publisher_tx, publisher_rx) = mpsc::channel(8);
    OtaPublisher::mock_new(client, publisher_rx);

    let mut ota = Ota::mock_new(system_update, state_mock, publisher_tx);
    let result = ota.init().await;

    assert!(result.is_ok());
}
