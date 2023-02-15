use crate::protocol::{
    ConnAck, ConnectReturnCode, Packet, PingResp, PubAck, PubAckReason, PubComp, PubCompReason,
    PubRel, PubRelReason, Publish, PublishProperties, QoS, SubAck, SubscribeReasonCode, UnsubAck,
};
use crate::router::alertlog::{AlertError, AlertEvent};
use crate::router::graveyard::SavedState;
use crate::router::scheduler::{PauseReason, Tracker};
use crate::router::Forward;
use crate::segments::Position;
use crate::*;
use flume::{bounded, Receiver, RecvError, Sender, TryRecvError};
use slab::Slab;
use std::collections::{HashMap, HashSet, VecDeque};
use std::str::Utf8Error;
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use thiserror::Error;
use tracing::{debug, error, info, trace, warn};

use super::alertlog::{Alert, AlertLog};
use super::graveyard::Graveyard;
use super::iobufs::{Incoming, Outgoing};
use super::logs::{AckLog, DataLog};
use super::scheduler::{ScheduleReason, Scheduler};
use super::{
    packetid, Connection, DataRequest, Event, FilterIdx, GetMeter, Meter, MetricsReply,
    MetricsRequest, Notification, RouterMeter, ShadowRequest, MAX_CHANNEL_CAPACITY,
    MAX_SCHEDULE_ITERATIONS,
};

#[derive(Error, Debug)]
pub enum RouterError {
    #[error("Receive error = {0}")]
    Recv(#[from] RecvError),
    #[error("Try Receive error = {0}")]
    TryRecv(#[from] TryRecvError),
    #[error("Disconnection")]
    Disconnected,
    #[error("Topic not utf-8")]
    NonUtf8Topic(#[from] Utf8Error),
    #[cfg(feature = "validate-tenant-prefix")]
    #[error("Bad Tenant")]
    BadTenant(String, String),
    #[error("No matching filters to topic {0}")]
    NoMatchingFilters(String),
    #[error("Unsupported QoS {0:?}")]
    UnsupportedQoS(QoS),
    #[error("Invalid filter prefix {0}")]
    InvalidFilterPrefix(Filter),
    #[error("Invalid client_id {0}")]
    InvalidClientId(String),
}

type FilterWithOffsets = Vec<(Filter, Offset)>;

pub struct Router {
    id: RouterId,
    /// Id of this router. Used to index native commitlog to store data from
    /// local connections
    config: RouterConfig,
    /// Saved state of dead persistent connections
    graveyard: Graveyard,
    /// List of MetersLink's senders
    meters: Slab<Sender<(ConnectionId, Vec<Meter>)>>,
    /// List of AlertsLink's senders with their respective subscription Filter
    alerts: Slab<(FilterWithOffsets, Sender<(ConnectionId, Alert)>)>,
    /// List of connections
    connections: Slab<Connection>,
    /// Connection map from device id to connection id
    connection_map: HashMap<String, ConnectionId>,
    /// Subscription map to interested connection ids
    subscription_map: HashMap<Filter, HashSet<ConnectionId>>,
    /// Incoming data grouped by connection
    ibufs: Slab<Incoming>,
    /// Outgoing data grouped by connection
    obufs: Slab<Outgoing>,
    /// Data log of all the subscriptions
    datalog: DataLog,
    /// Data log of all the alert subscriptions
    alertlog: AlertLog,
    /// Acks log per connection
    ackslog: Slab<AckLog>,
    /// Scheduler to schedule connections
    scheduler: Scheduler,
    /// Parked requests that are ready because of new data on the subscription
    notifications: VecDeque<(ConnectionId, DataRequest)>,
    /// Channel receiver to receive data from all the active connections and
    /// replicators. Each connection will have a tx handle which they use
    /// to send data and requests to router
    router_rx: Receiver<(ConnectionId, Event)>,
    /// Channel sender to send data to this router. This is given to active
    /// network connections, local connections and replicators to communicate
    /// with this router
    router_tx: Sender<(ConnectionId, Event)>,
    /// Router metrics
    router_meters: RouterMeter,
    /// Buffer for cache exchange of incoming packets
    cache: Option<VecDeque<Packet>>,
}

impl Router {
    pub fn new(router_id: RouterId, config: RouterConfig) -> Router {
        let (router_tx, router_rx) = bounded(1000);

        let meters = Slab::with_capacity(10);
        let alerts = Slab::with_capacity(10);
        let connections = Slab::with_capacity(config.max_connections);
        let ibufs = Slab::with_capacity(config.max_connections);
        let obufs = Slab::with_capacity(config.max_connections);
        let ackslog = Slab::with_capacity(config.max_connections);

        let router_metrics = RouterMeter {
            router_id,
            ..RouterMeter::default()
        };

        let max_connections = config.max_connections;
        Router {
            id: router_id,
            config: config.clone(),
            graveyard: Graveyard::new(),
            meters,
            alerts,
            connections,
            connection_map: Default::default(),
            subscription_map: Default::default(),
            ibufs,
            obufs,
            datalog: DataLog::new(config.clone()).unwrap(),
            alertlog: AlertLog::new(config).unwrap(),
            ackslog,
            scheduler: Scheduler::with_capacity(max_connections),
            notifications: VecDeque::with_capacity(1024),
            router_rx,
            router_tx,
            router_meters: router_metrics,
            cache: Some(VecDeque::with_capacity(MAX_CHANNEL_CAPACITY)),
        }
    }

    /// Gets handle to the router. This is not a public method to ensure that link
    /// is created only after the router starts
    fn link(&self) -> Sender<(ConnectionId, Event)> {
        self.router_tx.clone()
    }

    // pub(crate) fn get_replica_handle(&mut self, _replica_id: NodeId) -> (LinkTx, LinkRx) {
    //     unimplemented!()
    // }

    /// Starts the router in a background thread and returns link to it. Link
    /// to communicate with router should only be returned only after it starts.
    /// For that reason, all the public methods should start the router in the
    /// background
    #[tracing::instrument(skip_all)]
    pub fn spawn(mut self) -> Sender<(ConnectionId, Event)> {
        let router = thread::Builder::new().name(format!("router-{}", self.id));
        let link = self.link();
        router
            .spawn(move || {
                let e = self.run(0);
                error!(reason=?e, "Router done!");
            })
            .unwrap();
        link
    }

    /// Waits on incoming events when ready queue is empty.
    /// After pulling 1 event, tries to pull 500 more events
    /// before polling ready queue 100 times (connections)
    #[tracing::instrument(skip_all)]
    fn run(&mut self, count: usize) -> Result<(), RouterError> {
        match count {
            0 => loop {
                self.run_inner()?;
            },
            n => {
                for _ in 0..n {
                    self.run_inner()?;
                }
            }
        };

        Ok(())
    }

    fn run_inner(&mut self) -> Result<(), RouterError> {
        // Block on incoming events if there are no ready connections for consumption
        if self.consume().is_none() {
            // trace!("{}:: {:20} {:20} {:?}", self.id, "", "done-await", self.readyqueue);
            let (id, data) = self.router_rx.recv()?;
            self.events(id, data);
        }

        // Try reading more from connections in a non-blocking
        // fashion to accumulate data and handle subscriptions.
        // Accumulating more data lets requests retrieve bigger
        // bulks which in turn increases efficiency
        for _ in 0..500 {
            // All these methods will handle state and errors
            match self.router_rx.try_recv() {
                Ok((id, data)) => self.events(id, data),
                Err(TryRecvError::Disconnected) => return Err(RouterError::Disconnected),
                Err(TryRecvError::Empty) => break,
            }
        }

        // A connection should not be scheduled multiple times
        #[cfg(debug_assertions)]
        if let Some(readyqueue) = self.scheduler.check_readyqueue_duplicates() {
            warn!(
                "Connection was scheduled multiple times in readyqueue: {:?}",
                readyqueue
            );
        }

        // Poll 100 connections which are ready in ready queue
        for _ in 0..100 {
            self.consume();
        }

        self.send_all_alerts();
        Ok(())
    }

    fn events(&mut self, id: ConnectionId, data: Event) {
        let span = tracing::info_span!("[>] incoming", connection_id = id);
        let _guard = span.enter();

        match data {
            Event::Connect {
                connection,
                incoming,
                outgoing,
            } => self.handle_new_connection(connection, incoming, outgoing),
            Event::NewMeter(tx) => self.handle_new_meter(tx),
            Event::GetMeter(meter) => self.handle_get_meter(id, meter),
            Event::NewAlert(tx, filters) => self.handle_new_alert(tx, filters),
            Event::DeviceData => self.handle_device_payload(id),
            Event::Disconnect(disconnect) => self.handle_disconnection(id, disconnect.execute_will),
            Event::Ready => self.scheduler.reschedule(id, ScheduleReason::Ready),
            Event::Shadow(request) => {
                retrieve_shadow(&mut self.datalog, &mut self.obufs[id], request)
            }
            Event::Metrics(metrics) => retrieve_metrics(self, metrics),
        }
    }

    fn handle_new_connection(
        &mut self,
        mut connection: Connection,
        incoming: Incoming,
        outgoing: Outgoing,
    ) {
        let client_id = outgoing.client_id.clone();
        if let Err(err) = validate_clientid(&client_id) {
            error!("Invalid client_id: {}", err);
            return;
        };

        let span = tracing::info_span!("incoming_connect", client_id);
        let _guard = span.enter();

        // Check if same client_id already exists and if so, replace it with this new connection
        // ref: https://docs.oasis-open.org/mqtt/mqtt/v3.1.1/os/mqtt-v3.1.1-os.html#_Toc398718032

        let connection_id = self.connection_map.get(&client_id);
        if let Some(connection_id) = connection_id {
            error!(
                "Duplicate client_id, dropping previous connection with connection_id: {}",
                connection_id
            );
            self.handle_disconnection(*connection_id, true);
        }

        if self.connections.len() >= self.config.max_connections {
            error!("no space for new connection");
            // let ack = ConnectionAck::Failure("No space for new connection".to_owned());
            // let message = Notification::ConnectionAck(ack);
            return;
        }

        // Retrieve previous connection state from graveyard
        let saved = self.graveyard.retrieve(&client_id);
        let clean_session = connection.clean;
        let previous_session = saved.is_some();
        let tracker = if !clean_session {
            let saved = saved.map_or(SavedState::new(client_id.clone()), |s| s);
            connection.subscriptions = saved.subscriptions;
            connection.events = saved.metrics;
            saved.tracker
        } else {
            // Only retrieve metrics in clean session
            let saved = saved.map_or(SavedState::new(client_id.clone()), |s| s);
            connection.events = saved.metrics;
            Tracker::new(client_id.clone())
        };
        let ackslog = AckLog::new();

        let time = match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
            Ok(v) => v.as_millis().to_string(),
            Err(e) => format!("Time error = {e:?}"),
        };

        let event = "connection at ".to_owned() + &time + ", clean = " + &clean_session.to_string();
        connection.events.events.push_back(event);

        if connection.events.events.len() > 10 {
            connection.events.events.pop_front();
        }

        let connection_id = self.connections.insert(connection);
        assert_eq!(self.ibufs.insert(incoming), connection_id);
        assert_eq!(self.obufs.insert(outgoing), connection_id);

        self.connection_map.insert(client_id.clone(), connection_id);
        info!(connection_id, "Client connection registered");

        assert_eq!(self.ackslog.insert(ackslog), connection_id);
        assert_eq!(self.scheduler.add(tracker), connection_id);

        // Check if there are multiple data requests on same filter.
        debug_assert!(self
            .scheduler
            .check_tracker_duplicates(connection_id)
            .is_none());

        let alert = Alert::event(client_id.clone(), AlertEvent::Connect);
        match append_to_alertlog(alert, &mut self.alertlog) {
            Ok(_offset) => {}
            Err(e) => {
                error!(
                    reason = ?e, "Failed to append to alertlog"
                );
            }
        }

        let ack = ConnAck {
            session_present: !clean_session && previous_session,
            code: ConnectReturnCode::Success,
        };

        let ackslog = self.ackslog.get_mut(connection_id).unwrap();
        ackslog.connack(connection_id, ack);

        self.scheduler
            .reschedule(connection_id, ScheduleReason::Init);

        self.router_meters.total_connections += 1;
    }

    fn handle_new_meter(&mut self, tx: Sender<(ConnectionId, Vec<Meter>)>) {
        let meter_id = self.meters.insert(tx);
        let tx = &self.meters[meter_id];
        let _ = tx.try_send((
            meter_id,
            vec![Meter::Router(self.id, self.router_meters.clone())],
        ));
    }

    fn handle_new_alert(&mut self, tx: Sender<(ConnectionId, Alert)>, filters: Vec<Filter>) {
        let mut filter_with_offsets = Vec::new();
        for filter in filters {
            let offset = self.prepare_alert_filter(filter.to_string());
            filter_with_offsets.push((filter, offset));
        }
        let alert_id = self.alerts.insert((filter_with_offsets, tx));
        let (_, tx) = self.alerts.get(alert_id).unwrap();
        let _ = tx.try_send((
            alert_id,
            Alert::event("AlertsLink".to_owned(), AlertEvent::Connect),
        ));
    }

    fn handle_disconnection(&mut self, id: ConnectionId, execute_last_will: bool) {
        // Some clients can choose to send Disconnect packet before network disconnection.
        // This will lead to double Disconnect packets in router `events`
        let client_id = match &self.obufs.get(id) {
            Some(v) => v.client_id.clone(),
            None => {
                error!("no-connection id {} is already gone", id);
                return;
            }
        };
        let alert = Alert::event(client_id.clone(), AlertEvent::Disconnect);
        match append_to_alertlog(alert, &mut self.alertlog) {
            Ok(_offset) => {}
            Err(e) => {
                error!(
                    reason = ?e, "Failed to append to alertlog"
                );
            }
        }

        let span = tracing::info_span!("incoming_disconnect", client_id);
        let _guard = span.enter();

        if execute_last_will {
            self.handle_last_will(id);
        }

        info!("Disconnecting connection");

        // Remove connection from router
        let mut connection = self.connections.remove(id);
        let _incoming = self.ibufs.remove(id);
        let outgoing = self.obufs.remove(id);
        let mut tracker = self.scheduler.remove(id);
        self.connection_map.remove(&client_id);
        self.ackslog.remove(id);

        // Don't remove connection id from readyqueue with index. This will
        // remove wrong connection from readyqueue. Instead just leave diconnected
        // connection in readyqueue and allow 'consume()' method to deal with this
        // self.readyqueue.remove(id);

        let inflight_data_requests = self.datalog.clean(id);
        let retransmissions = outgoing.retransmission_map();

        // Remove this connection from subscriptions
        for filter in connection.subscriptions.iter() {
            if let Some(connections) = self.subscription_map.get_mut(filter) {
                connections.remove(&id);
            }
        }

        // Add disconnection event to metrics
        let time = match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
            Ok(v) => v.as_millis().to_string(),
            Err(e) => format!("Time error = {e:?}"),
        };

        let event = "disconnection at ".to_owned() + &time;
        connection.events.events.push_back(event);

        if connection.events.events.len() > 10 {
            connection.events.events.pop_front();
        }

        // Save state for persistent sessions
        if !connection.clean {
            // Add inflight data requests back to tracker
            inflight_data_requests
                .into_iter()
                .for_each(|r| tracker.register_data_request(r));

            for request in tracker.data_requests.iter_mut() {
                if let Some(cursor) = retransmissions.get(&request.filter_idx) {
                    request.cursor = *cursor;
                }
            }

            self.graveyard
                .save(tracker, connection.subscriptions, connection.events);
        } else {
            // Only save metrics in clean session
            self.graveyard
                .save(Tracker::new(client_id), HashSet::new(), connection.events);
        }
        self.router_meters.total_connections -= 1;
    }

    /// Handles new incoming data on a topic
    fn handle_device_payload(&mut self, id: ConnectionId) {
        // TODO: Retun errors and move error handling to the caller
        let incoming = match self.ibufs.get_mut(id) {
            Some(v) => v,
            None => {
                error!("no-connection id {} is already gone", id);
                return;
            }
        };

        let client_id = incoming.client_id.clone();
        let span = tracing::info_span!("incoming_payload", client_id);
        let _guard = span.enter();

        // Instead of exchanging, we should just append new incoming packets inside cache
        let mut packets = incoming.exchange(self.cache.take().unwrap());

        let mut force_ack = false;
        let mut new_data = false;
        let mut disconnect = false;
        let mut execute_will = true;

        // info!("{:15.15}[I] {:20} count = {}", client_id, "packets", packets.len());

        for packet in packets.drain(0..) {
            match packet {
                Packet::Publish(publish, props) => {
                    let span =
                        tracing::info_span!("publish", topic = ?publish.topic, pkid = publish.pkid);
                    let _guard = span.enter();

                    let qos = publish.qos;
                    let pkid = publish.pkid;

                    // Prepare acks for the above publish
                    // If any of the publish in the batch results in force flush,
                    // set global force flush flag. Force flush is triggered when the
                    // router is in instant ack more or connection data is from a replica
                    //
                    // TODO: handle multiple offsets
                    //
                    // The problem with multiple offsets is that when using replication with the current
                    // architecture, a single publish might get appended to multiple commit logs, resulting in
                    // multiple offsets (see `append_to_commitlog` function), meaning replicas will need to
                    // coordinate using multiple offsets, and we don't have any idea how to do so right now.
                    // Currently as we don't have replication, we just use a single offset, even when appending to
                    // multiple commit logs.

                    match qos {
                        QoS::AtLeastOnce => {
                            let puback = PubAck {
                                pkid,
                                reason: PubAckReason::Success,
                            };

                            let ackslog = self.ackslog.get_mut(id).unwrap();
                            ackslog.puback(puback);
                            force_ack = true;
                        }
                        QoS::ExactlyOnce => {
                            error!("QoS::ExactlyOnce is not yet supported");
                            disconnect = true;
                            break;
                            // let pubrec = PubRec {
                            //     pkid,
                            //     reason: PubRecReason::Success,
                            // };
                            //
                            // let ackslog = self.ackslog.get_mut(id).unwrap();
                            // ackslog.pubrec(publish, pubrec);
                            // force_ack = true;
                            // continue;
                        }
                        QoS::AtMostOnce => {
                            // Do nothing
                        }
                    };

                    self.router_meters.total_publishes += 1;

                    // Try to append publish to commitlog
                    match append_to_commitlog(
                        id,
                        publish.clone(),
                        props,
                        &mut self.datalog,
                        &mut self.notifications,
                        &mut self.connections,
                    ) {
                        Ok(_offset) => {
                            // Even if one of the data in the batch is appended to commitlog,
                            // set new data. This triggers notifications to wake waiters.
                            // Don't overwrite this flag to false if it is already true.
                            new_data = true;
                        }
                        Err(e) => {
                            // Disconnect on bad publishes
                            error!(
                                reason = ?e, "Failed to append to commitlog"
                            );
                            self.router_meters.failed_publishes += 1;
                            disconnect = true;
                            break;
                        }
                    };

                    let meter = &mut self.ibufs.get_mut(id).unwrap().meter;
                    if let Err(e) = meter.register_publish(&publish) {
                        error!(
                            reason = ?e, "Failed to write to incoming meter"
                        );
                    };
                }
                Packet::Subscribe(subscribe, _) => {
                    let mut return_codes = Vec::new();
                    let pkid = subscribe.pkid;
                    // let len = s.len();

                    for f in &subscribe.filters {
                        let span =
                            tracing::info_span!("subscribe", topic = f.path, pkid = subscribe.pkid);
                        let _guard = span.enter();

                        info!("Adding subscription on topic {}", f.path);
                        let connection = self.connections.get_mut(id).unwrap();

                        if let Err(e) = validate_subscription(connection, f) {
                            warn!(reason = ?e,"Subscription cannot be validated: {}", e);

                            disconnect = true;
                            break;
                        }

                        let filter = &f.path;
                        let qos = f.qos;

                        let (idx, cursor) = self.datalog.next_native_offset(filter);
                        self.prepare_filter(id, cursor, idx, filter.clone(), qos as u8);
                        self.datalog
                            .handle_retained_messages(filter, &mut self.notifications);

                        let code = match qos {
                            QoS::AtMostOnce => SubscribeReasonCode::QoS0,
                            QoS::AtLeastOnce => SubscribeReasonCode::QoS1,
                            QoS::ExactlyOnce => SubscribeReasonCode::QoS2,
                        };

                        return_codes.push(code);
                    }

                    let alert = Alert::event(client_id.clone(), AlertEvent::Subscribe(subscribe));
                    match append_to_alertlog(alert, &mut self.alertlog) {
                        Ok(_offset) => {
                            new_data = true;
                        }
                        Err(e) => {
                            error!(
                                reason = ?e, "Failed to append to alertlog"
                            );
                        }
                    }

                    // let meter = &mut self.ibufs.get_mut(id).unwrap().meter;
                    // meter.total_size += len;

                    let suback = SubAck { pkid, return_codes };
                    let ackslog = self.ackslog.get_mut(id).unwrap();
                    ackslog.suback(suback);
                    force_ack = true;
                }
                Packet::Unsubscribe(unsubscribe, _) => {
                    let connection = self.connections.get_mut(id).unwrap();
                    let pkid = unsubscribe.pkid;
                    for filter in &unsubscribe.filters {
                        let span = tracing::info_span!("unsubscribe", topic = filter, pkid);
                        let _guard = span.enter();

                        debug!("Removing subscription on filter {}", filter);
                        if let Some(connection_ids) = self.subscription_map.get_mut(filter) {
                            let removed = connection_ids.remove(&id);
                            if !removed {
                                continue;
                            }

                            let meter = &mut self.ibufs.get_mut(id).unwrap().meter;
                            meter.unregister_subscription(filter);

                            if !connection.subscriptions.remove(filter) {
                                warn!(
                                    pkid = unsubscribe.pkid,
                                    "Unsubscribe failed as filter was not subscribed previously"
                                );
                                continue;
                            }

                            let unsuback = UnsubAck {
                                pkid,
                                // reasons are used in MQTTv5
                                reasons: vec![],
                            };
                            let ackslog = self.ackslog.get_mut(id).unwrap();
                            ackslog.unsuback(unsuback);
                            self.scheduler.untrack(id, filter);
                            self.datalog.remove_waiters_for_id(id, filter);
                            force_ack = true;
                        }
                    }
                    let alert =
                        Alert::event(client_id.clone(), AlertEvent::Unsubscribe(unsubscribe));
                    match append_to_alertlog(alert, &mut self.alertlog) {
                        Ok(_offset) => {
                            new_data = true;
                        }
                        Err(e) => {
                            error!(
                                reason = ?e, "Failed to append to alertlog"
                            );
                        }
                    }
                }
                Packet::PubAck(puback, _) => {
                    let span = tracing::info_span!("puback", pkid = puback.pkid);
                    let _guard = span.enter();

                    let outgoing = self.obufs.get_mut(id).unwrap();
                    let pkid = puback.pkid;
                    if outgoing.register_ack(pkid).is_none() {
                        error!(pkid, "Unsolicited/ooo ack received for pkid {}", pkid);
                        disconnect = true;
                        break;
                    }

                    self.scheduler.reschedule(id, ScheduleReason::IncomingAck);
                }
                Packet::PubRec(pubrec, _) => {
                    let span = tracing::info_span!("pubrec", pkid = pubrec.pkid);
                    let _guard = span.enter();

                    let outgoing = self.obufs.get_mut(id).unwrap();
                    let pkid = pubrec.pkid;
                    if outgoing.register_ack(pkid).is_none() {
                        error!(pkid, "Unsolicited/ooo ack received for pkid {}", pkid);
                        disconnect = true;
                        break;
                    }

                    let ackslog = self.ackslog.get_mut(id).unwrap();
                    let pubrel = PubRel {
                        pkid: pubrec.pkid,
                        reason: PubRelReason::Success,
                    };

                    ackslog.pubrel(pubrel);
                    self.scheduler.reschedule(id, ScheduleReason::IncomingAck);
                }
                Packet::PubRel(pubrel, None) => {
                    let span = tracing::info_span!("pubrel", pkid = pubrel.pkid);
                    let _guard = span.enter();

                    let ackslog = self.ackslog.get_mut(id).unwrap();
                    let pubcomp = PubComp {
                        pkid: pubrel.pkid,
                        reason: PubCompReason::Success,
                    };

                    let publish = match ackslog.pubcomp(pubcomp) {
                        Some(v) => v,
                        None => {
                            disconnect = true;
                            break;
                        }
                    };

                    // Try to append publish to commitlog
                    match append_to_commitlog(
                        id,
                        publish,
                        None,
                        &mut self.datalog,
                        &mut self.notifications,
                        &mut self.connections,
                    ) {
                        Ok(_offset) => {
                            // Even if one of the data in the batch is appended to commitlog,
                            // set new data. This triggers notifications to wake waiters.
                            // Don't overwrite this flag to false if it is already true.
                            new_data = true;
                        }
                        Err(e) => {
                            // Disconnect on bad publishes
                            error!(
                                reason = ?e, "Failed to append to commitlog"
                            );
                            self.router_meters.failed_publishes += 1;
                            disconnect = true;
                            break;
                        }
                    };
                }
                Packet::PubComp(_pubcomp, _) => {}
                Packet::PingReq(_) => {
                    let ackslog = self.ackslog.get_mut(id).unwrap();
                    ackslog.pingresp(PingResp);

                    force_ack = true;
                }
                Packet::Disconnect(_, _) => {
                    let span = tracing::info_span!("disconnect");
                    let _guard = span.enter();

                    let alert = Alert::event(client_id, AlertEvent::Disconnect);
                    match append_to_alertlog(alert, &mut self.alertlog) {
                        Ok(_offset) => {
                            new_data = true;
                        }
                        Err(e) => {
                            error!(
                                reason = ?e, "Failed to append to alertlog"
                            );
                        }
                    }
                    disconnect = true;
                    execute_will = false;
                    break;
                }
                incoming => {
                    warn!(packet=?incoming, "Packet not supported by router yet" );
                }
            }
        }

        self.cache = Some(packets);

        // Prepare AcksRequest in tracker if router is operating in a
        // single node mode or force ack request for subscriptions
        if force_ack {
            self.scheduler.reschedule(id, ScheduleReason::FreshData);
        }

        // Notify waiting consumers only if there is publish data. During
        // subscription, data request is added to data waiter. With out this
        // if condition, connection will be woken up even during subscription
        if new_data {
            // Prepare all the consumers which are waiting for new data
            while let Some((id, request)) = self.notifications.pop_front() {
                self.scheduler.track(id, request);
                self.scheduler.reschedule(id, ScheduleReason::FreshData);
            }
        }

        // Incase BytesMut represents 10 packets, publish error/diconnect event
        // on say 5th packet should not block new data notifications for packets
        // 1 - 4. Hence we use a flag instead of diconnecting immediately
        if disconnect {
            self.handle_disconnection(id, execute_will);
        }
    }

    /// Apply filter and prepare this connection to receive subscription data
    fn prepare_alert_filter(&mut self, filter: String) -> Offset {
        let (_, offset) = self.alertlog.next_native_offset(&filter);
        offset
    }

    /// Apply filter and prepare this connection to receive subscription data
    fn prepare_filter(
        &mut self,
        id: ConnectionId,
        cursor: Offset,
        filter_idx: FilterIdx,
        filter: String,
        qos: u8,
    ) {
        // Add connection id to subscription list
        match self.subscription_map.get_mut(&filter) {
            Some(connections) => {
                connections.insert(id);
            }
            None => {
                let mut connections = HashSet::new();
                connections.insert(id);
                self.subscription_map.insert(filter.clone(), connections);
            }
        }

        // Prepare consumer to pull data in case of subscription
        let connection = self.connections.get_mut(id).unwrap();

        if connection.subscriptions.insert(filter.clone()) {
            let request = DataRequest {
                filter: filter.clone(),
                filter_idx,
                qos,
                cursor,
                read_count: 0,
                max_count: 100,
            };

            self.scheduler.track(id, request);
            self.scheduler.reschedule(id, ScheduleReason::NewFilter);
            debug_assert!(self.scheduler.check_tracker_duplicates(id).is_none())
        }

        let meter = &mut self.ibufs.get_mut(id).unwrap().meter;
        meter.register_subscription(filter);
    }

    /// When a connection is ready, it should sweep native data from 'datalog',
    /// send data and notifications to consumer.
    /// To activate a connection, first connection's tracker is fetched and
    /// all the requests are handled.
    fn consume(&mut self) -> Option<()> {
        let (id, mut requests) = self.scheduler.poll()?;

        let span = tracing::info_span!("[<] outgoing", connection_id = id);
        let _guard = span.enter();

        let outgoing = match self.obufs.get_mut(id) {
            Some(v) => v,
            None => {
                error!("Connection is already disconnected");
                return Some(());
            }
        };

        let ackslog = self.ackslog.get_mut(id).unwrap();
        let datalog = &mut self.datalog;
        let alertlog = &mut self.alertlog;

        trace!("Consuming requests");

        // We always try to ack when ever a connection is scheduled
        ack_device_data(ackslog, outgoing);

        // A new connection's tracker is always initialized with acks request.
        // A subscribe will register data request.
        // So a new connection is always scheduled with at least one request
        for _ in 0..MAX_SCHEDULE_ITERATIONS {
            let mut request = match requests.pop_front() {
                // Handle next data or acks request
                Some(request) => request,
                // No requests in the queue. This implies that consumer data and
                // acks are completely caught up. Pending requests are registered
                // in waiters and awaiting new notifications (device or replica data)
                None => {
                    self.scheduler.pause(id, PauseReason::Caughtup);
                    return Some(());
                }
            };

            match forward_device_data(&mut request, datalog, outgoing, alertlog) {
                ConsumeStatus::BufferFull => {
                    requests.push_back(request);
                    self.scheduler.pause(id, PauseReason::Busy);
                    break;
                }
                ConsumeStatus::InflightFull => {
                    requests.push_back(request);
                    self.scheduler.pause(id, PauseReason::InflightFull);
                    break;
                }
                ConsumeStatus::FilterCaughtup => {
                    let filter = &request.filter;
                    trace!(filter, "Filter caughtup {filter}, parking connection");

                    // When all the data in the log is caught up, current request is
                    // registered in waiters and not added back to the tracker. This
                    // ensures that tracker.next() stops when all the requests are done
                    datalog.park(id, request);
                }
                ConsumeStatus::PartialRead => {
                    requests.push_back(request);
                }
            }
        }

        // Add requests back to the tracker if there are any
        self.scheduler.trackv(id, requests);
        Some(())
    }

    pub fn handle_last_will(&mut self, id: ConnectionId) {
        let connection = self.connections.get_mut(id).unwrap();
        let will = match connection.last_will.take() {
            Some(v) => v,
            None => return,
        };

        let publish = Publish {
            dup: false,
            qos: will.qos,
            retain: will.retain,
            topic: will.topic,
            pkid: 0,
            payload: will.message,
        };
        match append_to_commitlog(
            id,
            publish,
            None,
            &mut self.datalog,
            &mut self.notifications,
            &mut self.connections,
        ) {
            Ok(_offset) => {
                // Prepare all the consumers which are waiting for new data
                while let Some((id, request)) = self.notifications.pop_front() {
                    self.scheduler.track(id, request);
                    self.scheduler.reschedule(id, ScheduleReason::FreshData);
                }
            }
            Err(e) => {
                // Disconnect on bad publishes
                error!(
                    reason = ?e, "Failed to append to commitlog"
                );
                self.router_meters.failed_publishes += 1;
                // Removed disconnect = true from here because we disconnect anyways
            }
        };
    }

    fn handle_get_meter(&self, meter_id: ConnectionId, meter: router::GetMeter) {
        let meter_tx = &self.meters[meter_id];
        match meter {
            GetMeter::Router => {
                let router_meters = Meter::Router(self.id, self.router_meters.clone());
                let _ = meter_tx.try_send((meter_id, vec![router_meters]));
            }
            GetMeter::Connection(client_id) => {
                let connections = match client_id {
                    Some(client_id) => {
                        if let Some(connection_id) = self.connection_map.get(&client_id) {
                            vec![(client_id, connection_id)]
                        } else {
                            let meter = Meter::Connection("".to_owned(), None, None);
                            let _ = meter_tx.try_send((meter_id, vec![meter]));
                            return;
                        }
                    }
                    // send meters for all connections
                    None => self
                        .connection_map
                        .iter()
                        .map(|(k, v)| (k.to_owned(), v))
                        .collect(),
                };

                // Update metrics
                let mut meter = Vec::new();
                for (client_id, connection_id) in connections {
                    let incoming_meter = self.ibufs.get(*connection_id).map(|v| v.meter.clone());
                    let outgoing_meter = self.obufs.get(*connection_id).map(|v| v.meter.clone());
                    meter.push(Meter::Connection(client_id, incoming_meter, outgoing_meter));
                }

                let _ = meter_tx.try_send((meter_id, meter));
            }
            GetMeter::Subscription(filter) => {
                let filters = match filter {
                    Some(filter) => vec![filter],
                    None => self.subscription_map.keys().map(|k| k.to_owned()).collect(),
                };

                let mut meter = Vec::new();
                for filter in filters {
                    let subscription_meter = self.datalog.meter(&filter);
                    meter.push(Meter::Subscription(filter, subscription_meter));
                }

                let _ = meter_tx.try_send((meter_id, meter));
            }
        };
    }
    fn send_all_alerts(&mut self) {
        let span = tracing::info_span!("outgoing_alert");
        let _guard = span.enter();

        for (alert_id, (filter_with_offsets, alert_sender)) in &mut self.alerts {
            for (filter, offset) in filter_with_offsets {
                trace!("Reading from alertlog: {}", filter);
                let (alerts, next_offset) = self
                    .alertlog
                    .native_readv(filter.to_string(), *offset, 100)
                    .unwrap();
                for alert in alerts {
                    let res = alert_sender.try_send((alert_id, alert.0));
                    if res.is_err() {
                        error!(
                        "Cannot send Alert to the channel, Error: {:?}. Dropping alerts for alert_id: {}",
                        res, alert_id
                    );
                    }
                }
                *offset = next_offset;
            }
        }
    }
}

fn append_to_alertlog(alert: Alert, alertlog: &mut AlertLog) -> Result<Offset, RouterError> {
    let topic = alert.topic();

    let filter_idxs = alertlog.matches(&topic).expect("Should never be None");

    let mut o = (0, 0);
    for filter_idx in filter_idxs {
        let alertlog_entry = alertlog.native.get_mut(filter_idx).unwrap();
        let (offset, filter) = alertlog_entry.append(alert.clone());
        debug!(
            "Appended to alertlog: {}[{}, {})",
            filter, offset.0, offset.1,
        );

        o = offset;
    }

    // error!("{:15.15}[E] {:20} topic = {}", connections[id].client_id, "no-filter", topic);
    Ok(o)
}

fn append_to_commitlog(
    id: ConnectionId,
    mut publish: Publish,
    publish_props: Option<PublishProperties>,
    datalog: &mut DataLog,
    notifications: &mut VecDeque<(ConnectionId, DataRequest)>,
    connections: &mut Slab<Connection>,
) -> Result<Offset, RouterError> {
    let topic = std::str::from_utf8(&publish.topic)?;

    // Ensure that only clients associated with a tenant can publish to tenant's topic
    #[cfg(feature = "validate-tenant-prefix")]
    if let Some(tenant_prefix) = &connections[id].tenant_prefix {
        if !topic.starts_with(tenant_prefix) {
            return Err(RouterError::BadTenant(
                tenant_prefix.to_owned(),
                topic.to_owned(),
            ));
        }
    }

    let exp = if let Some(ref props) = publish_props {
        props
            .message_expiry_interval
            .map(|t| Instant::now() + Duration::from_secs(t as u64))
    } else {
        None
    };

    if publish.payload.is_empty() {
        datalog.remove_from_retained_publishes(topic.to_owned());
    } else if publish.retain {
        datalog.insert_to_retained_publishes(publish.clone(), publish_props, topic.to_owned());
    }

    publish.retain = false;
    let pkid = publish.pkid;

    let filter_idxs = datalog.matches(topic);

    // Create a dynamic filter if dynamic_filters are enabled for this connection
    let filter_idxs = match filter_idxs {
        Some(v) => v,
        None if connections[id].dynamic_filters => {
            let (idx, _cursor) = datalog.next_native_offset(topic);
            vec![idx]
        }
        None => return Err(RouterError::NoMatchingFilters(topic.to_owned())),
    };

    let mut o = (0, 0);
    for filter_idx in filter_idxs {
        let datalog = datalog.native.get_mut(filter_idx).unwrap();
        let (offset, filter) = datalog.append((publish.clone(), exp), notifications);
        debug!(
            pkid,
            "Appended to commitlog: {}[{}, {})", filter, offset.0, offset.1,
        );

        o = offset;
    }

    // error!("{:15.15}[E] {:20} topic = {}", connections[id].client_id, "no-filter", topic);
    Ok(o)
}

/// Sweep ackslog for all the pending acks.
/// We write everything to outgoing buf with out worrying about buffer size
/// because acks most certainly won't cause memory bloat
fn ack_device_data(ackslog: &mut AckLog, outgoing: &mut Outgoing) -> bool {
    let span = tracing::info_span!("outgoing_ack", client_id = outgoing.client_id);
    let _guard = span.enter();

    let acks = ackslog.readv();
    if acks.is_empty() {
        debug!("No acks pending");
        return false;
    }

    let mut count = 0;
    let mut buffer = outgoing.data_buffer.lock();

    // Unlike forwards, we are reading all the pending acks for a given connection.
    // At any given point of time, there can be a max of connection's buffer size
    for ack in acks.drain(..) {
        let pkid = packetid(&ack);
        trace!(pkid, "Ack added for pkid {}", pkid);
        let message = Notification::DeviceAck(ack);
        buffer.push_back(message);
        count += 1;
    }

    debug!(acks_count = count, "Acks sent to device");
    outgoing.handle.try_send(()).ok();
    true
}

enum ConsumeStatus {
    /// Limit for publishes on outgoing channel reached
    BufferFull,
    /// Limit for inflight publishes on outgoing channel reached
    InflightFull,
    /// All publishes on topic forwarded
    FilterCaughtup,
    /// Some publishes on topic have been forwarded
    PartialRead,
}

/// Sweep datalog from offset in DataRequest and updates DataRequest
/// for next sweep. Returns (busy, caughtup) status
/// Returned arguments:
/// 1. `busy`: whether the data request was completed or not.
/// 2. `done`: whether the connection was busy or not.
/// 3. `inflight_full`: whether the inflight requests were completely filled
fn forward_device_data(
    request: &mut DataRequest,
    datalog: &DataLog,
    outgoing: &mut Outgoing,
    alertlog: &mut AlertLog,
) -> ConsumeStatus {
    let span = tracing::info_span!("outgoing_publish", client_id = outgoing.client_id);
    let _guard = span.enter();
    trace!(
        "Reading from datalog: {}[{}, {}]",
        request.filter,
        request.cursor.0,
        request.cursor.1
    );

    let inflight_slots = if request.qos == 1 {
        let len = outgoing.free_slots();
        if len == 0 {
            trace!("Aborting read from datalog: inflight capacity reached");
            return ConsumeStatus::InflightFull;
        }

        len as u64
    } else {
        datalog.config.max_read_len
    };

    let (next, publishes) =
        match datalog.native_readv(request.filter_idx, request.cursor, inflight_slots) {
            Ok(v) => v,
            Err(e) => {
                error!(error = ?e, "Failed to read from commitlog {}", e);
                return ConsumeStatus::FilterCaughtup;
            }
        };

    let (start, next, caughtup) = match next {
        Position::Next { start, end } => (start, end, false),
        Position::Done { start, end } => (start, end, true),
    };

    if start != request.cursor {
        let error = format!(
            "Read cursor start jumped from {:?} to {:?} on {}",
            request.cursor, start, request.filter
        );

        warn!(
            request_cursor = ?request.cursor,
            start_cursor = ?start,
            error
        );

        let alert = Alert::error(outgoing.client_id.clone(), AlertError::CursorJump(error));
        match append_to_alertlog(alert, alertlog) {
            Ok(_offset) => (),
            Err(e) => {
                error!(
                    reason = ?e, "Failed to append to alertlog"
                );
            }
        }
    }

    trace!(
        "Read from commitlog, cursor = {}[{}, {}), read count = {}",
        request.filter,
        next.0,
        next.1,
        publishes.len()
    );

    let qos = request.qos;
    let filter_idx = request.filter_idx;
    request.read_count += publishes.len();
    request.cursor = next;
    // println!("{:?} {:?} {}", start, next, request.read_count);

    if publishes.is_empty() {
        return ConsumeStatus::FilterCaughtup;
    }

    // Fill and notify device data
    let forwards = publishes.into_iter().map(|(mut publish, offset)| {
        publish.qos = protocol::qos(qos).unwrap();
        Forward {
            cursor: offset,
            size: 0,
            publish,
        }
    });

    let (len, inflight) = outgoing.push_forwards(forwards, qos, filter_idx);

    debug!(
        inflight_count = inflight,
        forward_count = len,
        "Forwarding publishes, cursor = {}[{}, {}) forward count = {}",
        request.filter,
        request.cursor.0,
        request.cursor.1,
        len
    );

    if len >= MAX_CHANNEL_CAPACITY - 1 {
        debug!("Outgoing channel reached its capacity");
        outgoing.push_notification(Notification::Unschedule);
        outgoing.handle.try_send(()).ok();
        return ConsumeStatus::BufferFull;
    }

    outgoing.handle.try_send(()).ok();
    if caughtup {
        ConsumeStatus::FilterCaughtup
    } else {
        ConsumeStatus::PartialRead
    }
}

fn retrieve_shadow(datalog: &mut DataLog, outgoing: &mut Outgoing, shadow: ShadowRequest) {
    if let Some(reply) = datalog.shadow(&shadow.filter) {
        let publish = reply;
        let shadow_reply = router::ShadowReply {
            topic: publish.topic,
            payload: publish.payload,
        };

        // Fill notify shadow
        let message = Notification::Shadow(shadow_reply);
        let len = outgoing.push_notification(message);
        if len >= MAX_CHANNEL_CAPACITY - 1 {
            outgoing.push_notification(Notification::Unschedule);
        }
        outgoing.handle.try_send(()).ok();
    }
}

fn retrieve_metrics(router: &mut Router, metrics: MetricsRequest) {
    let message = match metrics {
        MetricsRequest::Config => MetricsReply::Config(router.config.clone()),
        MetricsRequest::Router => MetricsReply::Router(router.router_meters.clone()),
        MetricsRequest::Connection(id) => {
            let metrics = router.connection_map.get(&id).map(|v| {
                let c = router
                    .connections
                    .get(*v)
                    .map(|v| v.events.clone())
                    .unwrap();
                let t = router.scheduler.trackers.get(*v).cloned().unwrap();
                (c, t)
            });

            let metrics = match metrics {
                Some(v) => Some(v),
                None => router
                    .graveyard
                    .retrieve(&id)
                    .map(|v| (v.metrics, v.tracker)),
            };

            MetricsReply::Connection(metrics)
        }
        MetricsRequest::Subscriptions => {
            let metrics: HashMap<Filter, Vec<String>> = router
                .subscription_map
                .iter()
                .map(|(filter, connections)| {
                    let connections = connections
                        .iter()
                        .map(|id| router.obufs[*id].client_id.clone())
                        .collect();

                    (filter.to_owned(), connections)
                })
                .collect();

            MetricsReply::Subscriptions(metrics)
        }
        MetricsRequest::Subscription(filter) => {
            let metrics = router.datalog.meter(&filter);
            MetricsReply::Subscription(metrics)
        }
        MetricsRequest::Waiters(filter) => {
            let metrics = router.datalog.waiters(&filter).map(|v| {
                // Convert (connection id, data request) list to (device id, data request) list
                v.waiters()
                    .iter()
                    .map(|(id, request)| (router.obufs[*id].client_id.clone(), request.clone()))
                    .collect()
            });

            MetricsReply::Waiters(metrics)
        }
        MetricsRequest::ReadyQueue => {
            let metrics = router.scheduler.readyqueue.clone();
            MetricsReply::ReadyQueue(metrics)
        }
    };

    println!("{message:#?}");
}

fn validate_subscription(
    connection: &mut Connection,
    filter: &protocol::Filter,
) -> Result<(), RouterError> {
    trace!(
        "validate subscription = {}, tenant = {:?}",
        filter.path,
        connection.tenant_prefix
    );
    // Ensure that only client devices of the tenant can
    #[cfg(feature = "validate-tenant-prefix")]
    if let Some(tenant_prefix) = &connection.tenant_prefix {
        if !filter.path.starts_with(tenant_prefix) {
            return Err(RouterError::InvalidFilterPrefix(filter.path.to_owned()));
        }
    }

    if filter.qos == QoS::ExactlyOnce {
        return Err(RouterError::UnsupportedQoS(filter.qos));
    }

    if filter.path.starts_with('$') {
        return Err(RouterError::InvalidFilterPrefix(filter.path.to_owned()));
    }

    Ok(())
}

fn validate_clientid(client_id: &str) -> Result<(), RouterError> {
    trace!("Validating Client ID = {}", client_id,);
    // Ensure that only client devices of the tenant can
    if "+$#/".chars().any(|c| client_id.contains(c)) {
        return Err(RouterError::InvalidClientId(client_id.to_string()));
    }

    Ok(())
}

// #[cfg(test)]
// #[allow(non_snake_case)]
// mod test {
//     use std::{
//         thread,
//         time::{Duration, Instant},
//     };

//     use bytes::BytesMut;

//     use super::*;
//     use crate::{
//         link::local::*,
//         protocol::v4::{self, subscribe::SubscribeFilter, QoS},
//         router::ConnectionMeter,
//     };

//     /// Create a router and n connections
//     fn new_router(count: usize, clean: bool) -> (Router, VecDeque<(LinkTx, LinkRx)>) {
//         let config = RouterConfig {
//             instant_ack: true,
//             max_segment_size: 1024 * 10, // 10 KB
//             max_mem_segments: 10,
//             max_disk_segments: 0,
//             max_read_len: 128,
//             log_dir: None,
//             max_connections: 128,
//             dynamic_log: false,
//         };

//         let mut router = Router::new(0, config);
//         let link = router.link();
//         let handle = thread::spawn(move || {
//             (0..count)
//                 .map(|i| Link::new(&format!("link-{}", i), link.clone(), clean))
//                 .collect::<VecDeque<Result<_, _>>>()
//         });

//         router.run_exact(count).unwrap();
//         let links = handle
//             .join()
//             .unwrap()
//             .into_iter()
//             .map(|x| x.unwrap())
//             .collect();
//         (router, links)
//     }

//     fn reconnect(router: &mut Router, link_id: usize, clean: bool) -> (LinkTx, LinkRx) {
//         let link = router.link();
//         let handle =
//             thread::spawn(move || Link::new(&format!("link-{}", link_id), link, clean).unwrap());
//         router.run(1).unwrap();
//         handle.join().unwrap()
//     }

//     #[test]
//     fn test_graveyard_retreive_metrics_always() {
//         let (mut router, mut links) = new_router(1, false);
//         let (tx, _) = links.pop_front().unwrap();
//         let id = tx.connection_id;
//         let conn = router.connections.get_mut(id).unwrap();
//         conn.meter = ConnectionMeter {
//             publish_size: 1000,
//             publish_count: 1000,
//             subscriptions: HashSet::new(),
//             events: conn.meter.events.clone(),
//         };
//         conn.meter.push_event("graveyard-testing".to_string());
//         router.handle_disconnection(id);

//         let (tx, _) = reconnect(&mut router, 0, false);
//         let id = tx.connection_id;
//         let conn = router.connections.get_mut(id).unwrap();
//         assert_eq!(conn.meter.publish_size, 1000);
//         assert_eq!(conn.meter.publish_count, 1000);
//         assert_eq!(conn.meter.events.len(), 4);

//         let (mut router, mut links) = new_router(1, true);
//         let (tx, _) = links.pop_front().unwrap();
//         let id = tx.connection_id;
//         let conn = router.connections.get_mut(id).unwrap();
//         conn.meter = ConnectionMeter {
//             publish_size: 1000,
//             publish_count: 1000,
//             subscriptions: HashSet::new(),
//             events: conn.meter.events.clone(),
//         };
//         conn.meter.push_event("graveyard-testing".to_string());
//         router.handle_disconnection(id);

//         let (tx, _) = reconnect(&mut router, 0, true);
//         let id = tx.connection_id;
//         let conn = router.connections.get_mut(id).unwrap();
//         assert_eq!(conn.meter.publish_size, 1000);
//         assert_eq!(conn.meter.publish_count, 1000);
//         assert_eq!(conn.meter.events.len(), 4);
//     }

//     #[test]
//     #[should_panic]
//     fn test_blocking_too_many_connections() {
//         new_router(512, true);
//     }

//     #[test]
//     fn test_not_clean_sessions() {
//         // called once per data push (subscribe to 2 topics in our case)
//         // called once per each sub topic
//         // if prepare_data_request called, then Tracker::register_data_request is also called so
//         // no need to check separately for that

//         let (mut router, mut links) = new_router(2, false);
//         let (tx1, mut rx1) = links.pop_front().unwrap();
//         let id1 = tx1.connection_id;
//         let (tx2, mut rx2) = links.pop_front().unwrap();
//         let id2 = tx2.connection_id;

//         // manually subscribing to avoid calling Router::run(), so that we can see and test changes
//         // in Router::readyqueue.
//         let mut buf = BytesMut::new();
//         v4::subscribe::write(
//             vec![
//                 SubscribeFilter::new("hello/1/world".to_string(), QoS::AtMostOnce),
//                 SubscribeFilter::new("hello/2/world".to_string(), QoS::AtMostOnce),
//             ],
//             0,
//             &mut buf,
//         )
//         .unwrap();
//         let buf = buf.freeze();
//         {
//             let incoming = router.ibufs.get_mut(id1).unwrap();
//             let mut recv_buf = incoming.buffer.lock();
//             recv_buf.push_back(buf.clone());
//             assert_eq!(router.subscription_map.len(), 0);
//         }
//         {
//             let incoming = router.ibufs.get_mut(id2).unwrap();
//             let mut recv_buf = incoming.buffer.lock();
//             recv_buf.push_back(buf.clone());
//             assert_eq!(router.subscription_map.len(), 0);
//         }

//         router.handle_device_payload(id1);
//         assert_eq!(router.subscription_map.len(), 2);
//         assert_eq!(router.readyqueue.len(), 1);
//         router.consume(id1);
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/1/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             1
//         );
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/2/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             1
//         );

//         router.handle_device_payload(id2);
//         assert_eq!(router.subscription_map.len(), 2);
//         assert_eq!(router.readyqueue.len(), 2);
//         router.consume(id2);
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/1/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             2
//         );
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/2/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             2
//         );

//         assert!(matches!(rx1.recv().unwrap(), Some(Notification::DeviceAck(_))));
//         assert!(matches!(rx2.recv().unwrap(), Some(Notification::DeviceAck(_))));

//         router.handle_disconnection(id1);
//         router.handle_disconnection(id2);
//         router.run(1).unwrap();

//         let (_, _) = reconnect(&mut router, 0, false);
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/1/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             1
//         );
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/2/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             1
//         );

//         let (_, _) = reconnect(&mut router, 1, false);
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/1/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             2
//         );
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/2/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             2
//         );
//     }

//     #[test]
//     fn test_publish_appended_to_commitlog() {
//         let (mut router, mut links) = new_router(2, true);
//         let (mut sub_tx, _) = links.pop_front().unwrap();
//         let (mut pub_tx, _) = links.pop_front().unwrap();

//         sub_tx.subscribe("hello/1/world").unwrap();
//         router.run(1).unwrap();

//         // Each LinkTx::publish creates a single new event
//         for _ in 0..10 {
//             pub_tx.publish("hello/1/world", b"hello".to_vec()).unwrap();
//         }
//         router.run(1).unwrap();
//         assert_eq!(router.datalog.next_native_offset("hello/1/world").1, (0, 10));
//     }

//     #[test]
//     fn test_adding_caughtups_to_waiters() {
//         // number of times forward_device_data hit
//         //     = router.run times * topics * subscribers per topic
//         //     = 2 * 2 * 2 = 8
//         //
//         // we also call consume 2 times per subsriber, and one time it will call
//         // forward_device_data again, but no data will actually be forwarded.
//         // as 256 publishes at once, and router's config only reads 128 at a time, data needs to be
//         // read from same log twice, and thus half of the times the data reading is not done

//         let (mut router, mut links) = new_router(4, true);
//         let (mut sub1_tx, _) = links.pop_front().unwrap();
//         let (mut pub1_tx, _) = links.pop_front().unwrap();
//         let (mut sub2_tx, _) = links.pop_front().unwrap();
//         let (mut pub2_tx, _) = links.pop_front().unwrap();
//         sub1_tx.subscribe("hello/1/world").unwrap();
//         sub1_tx.subscribe("hello/2/world").unwrap();
//         sub2_tx.subscribe("hello/1/world").unwrap();
//         sub2_tx.subscribe("hello/2/world").unwrap();
//         router.run(1).unwrap();

//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/1/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             2
//         );
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/2/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             2
//         );

//         for _ in 0..256 {
//             pub1_tx.publish("hello/1/world", b"hello".to_vec()).unwrap();
//             pub2_tx.publish("hello/2/world", b"hello".to_vec()).unwrap();
//         }
//         router.run(1).unwrap();

//         let tracker = router.trackers.get(sub1_tx.connection_id).unwrap();
//         assert_eq!(tracker.get_data_requests().len(), 2);
//         let tracker = router.trackers.get(sub2_tx.connection_id).unwrap();
//         assert_eq!(tracker.get_data_requests().len(), 2);

//         router.run(1).unwrap();

//         let tracker = router.trackers.get(sub1_tx.connection_id).unwrap();
//         assert_eq!(tracker.get_data_requests().len(), 2);
//         let tracker = router.trackers.get(sub2_tx.connection_id).unwrap();
//         assert_eq!(tracker.get_data_requests().len(), 2);

//         router.consume(sub1_tx.connection_id);
//         router.consume(sub2_tx.connection_id);

//         let tracker = router.trackers.get(sub1_tx.connection_id).unwrap();
//         assert_eq!(tracker.get_data_requests().len(), 1);
//         let tracker = router.trackers.get(sub2_tx.connection_id).unwrap();
//         assert_eq!(tracker.get_data_requests().len(), 1);

//         router.consume(sub1_tx.connection_id);
//         router.consume(sub2_tx.connection_id);

//         let tracker = router.trackers.get(sub1_tx.connection_id).unwrap();
//         assert_eq!(tracker.get_data_requests().len(), 0);
//         let tracker = router.trackers.get(sub2_tx.connection_id).unwrap();
//         assert_eq!(tracker.get_data_requests().len(), 0);

//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/1/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             2
//         );
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/2/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             2
//         );
//     }

//     #[test]
//     fn test_disconnect_invalid_sub() {
//         let (mut router, mut links) = new_router(3, true);

//         let (mut tx, mut rx) = links.pop_front().unwrap();
//         tx.subscribe("$hello/world").unwrap();
//         router.run(1).unwrap();
//         assert!(matches!(rx.recv(), Err(LinkError::Recv(flume::RecvError::Disconnected))));

//         let (mut tx, mut rx) = links.pop_front().unwrap();
//         tx.subscribe("test/hello").unwrap();
//         router.run(1).unwrap();
//         assert!(matches!(rx.recv(), Err(LinkError::Recv(flume::RecvError::Disconnected))));
//     }

//     #[test]
//     fn test_disconnect_invalid_pub() {
//         let (mut router, mut links) = new_router(1, true);
//         let (mut tx, mut rx) = links.pop_front().unwrap();
//         tx.publish("invalid/topic", b"hello".to_vec()).unwrap();
//         router.run(1).unwrap();
//         assert!(matches!(rx.recv(), Err(LinkError::Recv(flume::RecvError::Disconnected))));
//     }

//     #[test]
//     fn test_pingreq() {
//         let (mut router, mut links) = new_router(1, true);
//         let (mut tx, mut rx) = links.pop_front().unwrap();
//         let mut buf = BytesMut::new();
//         mqttbytes::v4::PingReq.write(&mut buf).unwrap();
//         let buf = buf.freeze();
//         for _ in 0..10 {
//             tx.push(buf.clone()).unwrap();
//         }
//         router.run(1).unwrap();
//         for _ in 0..10 {
//             let ret = rx.recv().unwrap().unwrap();
//             assert!(matches!(ret, Notification::DeviceAck(Ack::PingResp)));
//         }
//     }

//     #[test]
//     fn test_disconnect() {
//         let (mut router, mut links) = new_router(1, true);
//         let (mut tx, mut rx) = links.pop_front().unwrap();
//         let mut buf = BytesMut::new();
//         mqttbytes::v4::Disconnect.write(&mut buf).unwrap();
//         let buf = buf.freeze();
//         tx.push(buf).unwrap();
//         router.run(1).unwrap();
//         assert!(matches!(rx.recv(), Err(LinkError::Recv(flume::RecvError::Disconnected))));
//     }

//     #[test]
//     fn test_connection_in_qos1() {
//         // connection as in crate::connection::Connection struct should get filled for all unacked,
//         // and refuse to add more packets than unacked.

//         let config = RouterConfig {
//             instant_ack: true,
//             max_segment_size: 1024 * 10, // 10 KB
//             max_mem_segments: 10,
//             max_disk_segments: 0,
//             max_read_len: 256,
//             log_dir: None,
//             max_connections: 128,
//             dynamic_log: false,
//         };

//         let mut router = Router::new(0, config);
//         let link = router.link();
//         let handle = thread::spawn(move || {
//             (0..2)
//                 .map(|i| Link::new(&format!("link-{}", i), link.clone(), true))
//                 .collect::<VecDeque<Result<_, _>>>()
//         });

//         router.run(2).unwrap();
//         let mut links: VecDeque<(LinkTx, LinkRx)> = handle
//             .join()
//             .unwrap()
//             .into_iter()
//             .map(|x| x.unwrap())
//             .collect();

//         let (mut sub_tx, _sub_rx) = links.pop_front().unwrap();
//         let (mut pub_tx, _) = links.pop_front().unwrap();

//         let mut buf = BytesMut::new();
//         mqttbytes::v4::Subscribe::new("hello/1/world", mqttbytes::QoS::AtLeastOnce)
//             .write(&mut buf)
//             .unwrap();
//         let buf = buf.freeze();
//         sub_tx.push(buf).unwrap();
//         router.run(1).unwrap();

//         for _ in 0..202 {
//             pub_tx.publish("hello/1/world", b"hello".to_vec()).unwrap();
//         }
//         router.run(1).unwrap();

//         let id = sub_tx.connection_id;
//         let tracker = router.trackers.get(id).unwrap();
//         assert_eq!(tracker.get_data_requests().front().unwrap().cursor, (0, 200));
//     }

//     #[test]
//     #[ignore]
//     fn test_resend_in_qos1() {
//         unimplemented!("we don't resend QoS1 packets yet")
//     }

//     #[test]
//     fn test_wildcard_subs() {
//         let (mut router, mut links) = new_router(3, true);

//         let (mut sub1_tx, mut sub1_rx) = links.pop_front().unwrap();
//         sub1_tx.subscribe("#").unwrap();

//         let (mut sub2_tx, mut sub2_rx) = links.pop_front().unwrap();
//         sub2_tx.subscribe("hello/+/world").unwrap();

//         let (mut pub_tx, mut _pub_rx) = links.pop_front().unwrap();
//         for _ in 0..10 {
//             for i in 0..10 {
//                 pub_tx
//                     .publish(format!("hello/{}/world", i), b"hello".to_vec())
//                     .unwrap();
//             }
//         }

//         for _ in 0..10 {
//             pub_tx.publish("hello/world", b"hello".to_vec()).unwrap();
//         }

//         router.run(1).unwrap();

//         let mut count = 0;
//         while let Ok(Some(notification)) =
//             sub1_rx.recv_deadline(Instant::now() + Duration::from_millis(1))
//         {
//             match notification {
//                 Notification::Forward { .. } => count += 1,
//                 _ => {}
//             }
//         }
//         assert_eq!(count, 110);

//         count = 0;
//         while let Ok(Some(notification)) =
//             sub2_rx.recv_deadline(Instant::now() + Duration::from_millis(1))
//         {
//             match notification {
//                 Notification::Forward { .. } => count += 1,
//                 _ => {}
//             }
//         }
//         assert_eq!(count, 100);
//     }
// }

// // #[cfg(test)]
// // mod test {
// //     use crate::connection::Connection;
// //     use crate::link::local::{Link, LinkRx, LinkTx};
// //     use crate::tracker::Tracker;
// //     use crate::{ConnectionId, Notification, Router, RouterConfig};
// //     use bytes::BytesMut;
// //     use flume::Receiver;
// //     use mqttbytes::v4::{Publish, Subscribe};
// //     use mqttbytes::QoS;
// //     use std::collections::VecDeque;
// //     use std::sync::atomic::AtomicBool;
// //     use std::sync::Arc;
// //     use std::thread;

// //     /// Create a router and n connections
// //     fn router(count: usize) -> (Router, VecDeque<(LinkTx, LinkRx)>) {
// //         let config = RouterConfig {
// //             data_filter: "hello/world".to_owned(),
// //             wildcard_filters: vec![],
// //             instant_ack: true,
// //             max_segment_size: 10 * 1024,
// //             max_segment_count: 10 * 1024,
// //             max_connections: 10,
// //         };

// //         let mut router = Router::new(0, config);
// //         let link = router.link();
// //         let handle = thread::spawn(move || {
// //             (0..count)
// //                 .map(|i| Link::new(&format!("link-{}", i), link.clone()).unwrap())
// //                 .collect()
// //         });

// //         router.run(count).unwrap();
// //         let links = handle.join().unwrap();
// //         (router, links)
// //     }

// //     /// Creates a connection
// //     fn connection(client_id: &str, clean: bool) -> (Connection, Receiver<Notification>) {
// //         let (tx, rx) = flume::bounded(10);
// //         let connection = Connection::new(client_id, clean, tx, Arc::new(AtomicBool::new(false)));
// //         (connection, rx)
// //     }

// //     /// Raw publish message
// //     fn publish(topic: &str) -> BytesMut {
// //         let mut publish = Publish::new(topic, QoS::AtLeastOnce, vec![1, 2, 3]);
// //         publish.pkid = 1;

// //         let mut o = BytesMut::new();
// //         publish.write(&mut o).unwrap();
// //         o
// //     }

// //     /// Raw publish message
// //     fn subscribe(topic: &str) -> BytesMut {
// //         let mut subscribe = Subscribe::new(topic, QoS::AtLeastOnce);
// //         subscribe.pkid = 1;

// //         let mut o = BytesMut::new();
// //         subscribe.write(&mut o).unwrap();
// //         o
// //     }

// //     /// When there is new data on a subscription's commitlog, it's
// //     /// consumer should start triggering data requests to pull data
// //     /// from commitlog
// //     #[test]
// //     fn new_connection_data_triggers_acks_of_self_and_data_of_consumer() {
// //         let (mut router, mut links) = router(2);
// //         let (mut l1tx, _l1rx) = links.pop_front().unwrap();
// //         let (mut l2tx, l2rx) = links.pop_front().unwrap();

// //         l2tx.subscribe("hello/world").unwrap();
// //         l1tx.publish("hello/world", vec![1, 2, 3]).unwrap();
// //         let _ = router.run(1);

// //         // Suback
// //         match l2rx.recv().unwrap() {
// //             Notification::DeviceAcks(_) => {}
// //             v => panic!("{:?}", v),
// //         }

// //         // Consumer data
// //         match l2rx.recv().unwrap() {
// //             Notification::DeviceData { cursor, .. } => assert_eq!(cursor, (0, 1)),
// //             v => panic!("{:?}", v),
// //         }

// //         // No acks for qos0 publish
// //     }

// //     #[test]
// //     fn half_open_connections_are_handled_correctly() {
// //         let config = RouterConfig {
// //             data_filter: "hello/world".to_owned(),
// //             wildcard_filters: vec![],
// //             instant_ack: true,
// //             max_segment_size: 10 * 1024,
// //             max_segment_count: 10 * 1024,
// //             max_connections: 10,
// //         };

// //         let mut router = Router::new(0, config);
// //         let (c, rx) = connection("test", true);
// //         router.handle_new_connection(c);

// //         let id = *router.connection_map.get("test").unwrap();
// //         router.handle_device_payload(id, subscribe("hello/data"));
// //         router.handle_device_payload(id, publish("hello/data"));

// //         let trackers = router.trackers.get(id).unwrap();
// //         assert!(trackers.get_data_requests().len() > 0);
// //         dbg!(trackers);

// //         // A new connection with same client id
// //         let (c, rx) = connection("test", true);
// //         router.handle_new_connection(c);

// //         let id = *router.connection_map.get("test").unwrap();
// //         let trackers = router.trackers.get(id).unwrap();
// //         dbg!(trackers);
// //     }
// // }
