//
// Copyright (c) 2022 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//
pub(crate) mod accept;
pub mod authenticator;
pub(super) mod cookie;
pub(crate) mod open;
// pub(super) mod properties;

use super::super::TransportManager;
use super::{TransportConfigUnicast, TransportPeer, TransportUnicast};
use authenticator::AuthenticatedPeerLink;
use cookie::*;
// use properties::*;
use rand::Rng;
use std::time::Duration;
use zenoh_core::{zasynclock, zasyncread};
use zenoh_link::{Link, LinkUnicast};
use zenoh_protocol::{
    core::{Field, Resolution, WhatAmI, ZenohId},
    transport::{Close, TransportMessage},
};
use zenoh_result::ZResult;

pub(super) async fn close_link(
    link: &LinkUnicast,
    manager: &TransportManager,
    auth_link: &AuthenticatedPeerLink,
    mut reason: Option<u8>,
) {
    if let Some(reason) = reason.take() {
        // Build the close message
        let message: TransportMessage = Close {
            reason,
            session: false,
        }
        .into();
        // Send the close message on the link
        let _ = link.write_transport_message(&message).await;
    }

    // Close the link
    let _ = link.close().await;
    // Notify the authenticators
    for pa in zasyncread!(manager.state.unicast.peer_authenticator).iter() {
        pa.handle_link_err(auth_link).await;
    }
}

/*************************************/
/*            TRANSPORT              */
/*************************************/
pub(super) struct InputInit {
    pub(super) zid: ZenohId,
    pub(super) whatami: WhatAmI,
    pub(super) resolution: Resolution,
    pub(super) is_qos: bool,
}
async fn transport_init(
    manager: &TransportManager,
    input: self::InputInit,
) -> ZResult<TransportUnicast> {
    // Initialize the transport if it is new
    let tx_initial_sn =
        zasynclock!(manager.prng).gen_range(0..=input.resolution.get(Field::FrameSN).mask());

    let config = TransportConfigUnicast {
        peer: input.zid,
        whatami: input.whatami,
        sn_resolution: input.resolution.get(Field::FrameSN).mask(),
        tx_initial_sn,
        is_qos: input.is_qos,
        is_shm: false, // @TODO
    };

    manager.init_transport_unicast(config)
}

pub(super) struct InputFinalize {
    pub(super) transport: TransportUnicast,
    pub(super) lease: Duration,
    pub(super) tx_batch_size: u16,
}
// Finalize the transport, notify the callback and start the link tasks
pub(super) async fn transport_finalize(
    link: &LinkUnicast,
    manager: &TransportManager,
    input: self::InputFinalize,
) -> ZResult<()> {
    // Retrive the transport's transport
    let transport = input.transport.get_inner()?;

    // Start the TX loop
    let keep_alive = manager.config.unicast.lease / manager.config.unicast.keep_alive as u32;
    let batch_size = manager.config.batch_size.min(input.tx_batch_size);
    transport.start_tx(link, &manager.tx_executor, keep_alive, batch_size)?;

    // Assign a callback if the transport is new
    // Keep the lock to avoid concurrent new_transport and closing/closed notifications
    let a_guard = transport.get_alive().await;
    if transport.get_callback().is_none() {
        let peer = TransportPeer {
            zid: transport.get_zid(),
            whatami: transport.get_whatami(),
            is_qos: transport.is_qos(),
            is_shm: transport.is_shm(),
            links: vec![Link::from(link)],
        };
        // Notify the transport handler that there is a new transport and get back a callback
        // NOTE: the read loop of the link the open message was sent on remains blocked
        //       until new_unicast() returns. The read_loop in the various links
        //       waits for any eventual transport to associate to.
        let callback = manager
            .config
            .handler
            .new_unicast(peer, input.transport.clone())?;
        // Set the callback on the transport
        transport.set_callback(callback);
    }
    if let Some(callback) = transport.get_callback() {
        // Notify the transport handler there is a new link on this transport
        callback.new_link(Link::from(link));
    }
    drop(a_guard);

    // Start the RX loop
    transport.start_rx(link, input.lease, batch_size)?;

    Ok(())
}
