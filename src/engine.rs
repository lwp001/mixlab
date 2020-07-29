use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::f32;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender, Receiver, RecvTimeoutError, TrySendError, TryRecvError};
use std::thread;
use std::time::{Instant, Duration};

use futures::future;
use futures::stream::{Stream, StreamExt};
use tokio::runtime;
use tokio::sync::{oneshot, broadcast, watch};

use mixlab_protocol::{ModuleId, InputId, OutputId, ClientMessage, TerminalId, WorkspaceState, WindowGeometry, ServerUpdate, Indication, LineType, ClientSequence, ClientOp, PerformanceInfo};

use crate::module::Module;
use crate::util::Sequence;

mod timing;
use timing::{EngineStat, TickStat};

mod io;
pub use io::{InputRef, OutputRef, Output, VideoFrame};

mod save;

pub type Sample = f32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionId(NonZeroUsize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
// NOTE! This is not Ord because log positions with different session IDs have
// no relative ordering
pub struct OpClock(pub SessionId, pub ClientSequence);

impl PartialOrd for OpClock {
    fn partial_cmp(&self, other: &OpClock) -> Option<Ordering> {
        if self.0 == other.0 {
            Some(self.1.cmp(&other.1))
        } else {
            None
        }
    }
}

pub const CHANNELS: usize = 2;
pub const SAMPLE_RATE: usize = 44100;
pub const TICKS_PER_SECOND: usize = 60;
pub const SAMPLES_PER_TICK: usize = SAMPLE_RATE / TICKS_PER_SECOND;

pub enum EngineMessage {
    ConnectSession(oneshot::Sender<(SessionId, WorkspaceState, EngineEvents)>),
    ClientMessage(SessionId, ClientMessage),
}

pub struct EngineHandle {
    cmd_tx: SyncSender<EngineMessage>,
    perf_rx: watch::Receiver<Option<Arc<PerformanceInfo>>>,
}

pub struct EngineSession {
    session_id: SessionId,
    cmd_tx: SyncSender<EngineMessage>,
}

pub fn start(tokio_runtime: runtime::Handle) -> EngineHandle {
    let (cmd_tx, cmd_rx) = mpsc::sync_channel(8);
    let (log_tx, _) = broadcast::channel(64);
    let (perf_tx, perf_rx) = watch::channel(None);

    thread::spawn(move || {
        let mut engine = Engine {
            cmd_rx,
            log_tx,
            perf_tx,
            session_seq: Sequence::new(),
            workspace: Workspace::new(),
        };

        // enter the tokio runtime context for the engine thread
        // this allows modules to spawn async tasks
        tokio_runtime.enter(|| {
            engine.run();
        });
    });

    EngineHandle { cmd_tx, perf_rx }
}

#[derive(Debug)]
pub enum EngineError {
    Stopped,
    Busy,
}

impl<T> From<TrySendError<T>> for EngineError {
    fn from(e: TrySendError<T>) -> Self {
        match e {
            TrySendError::Full(_) => EngineError::Busy,
            TrySendError::Disconnected(_) => EngineError::Stopped,
        }
    }
}

pub type EngineEvents = broadcast::Receiver<EngineEvent>;

#[derive(Debug, Clone)]
pub enum EngineEvent {
    Sync(OpClock),
    ServerUpdate(ServerUpdate),
}

impl EngineHandle {
    pub async fn connect(&self) -> Result<(WorkspaceState, EngineEvents, EngineSession), EngineError> {
        let cmd_tx = self.cmd_tx.clone();

        let (tx, rx) = oneshot::channel();
        cmd_tx.try_send(EngineMessage::ConnectSession(tx))?;
        let (session_id, state, log_rx) = rx.await.map_err(|_| EngineError::Stopped)?;

        Ok((state, log_rx, EngineSession {
            cmd_tx,
            session_id,
        }))
    }

    pub fn performance_info(&self) -> impl Stream<Item = Arc<PerformanceInfo>> {
        self.perf_rx.clone().filter_map(|info| future::ready(info))
    }
}

impl EngineSession {
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// TODO - maybe pass log position in here and detect conflicts?
    pub fn update(&self, msg: ClientMessage) -> Result<(), EngineError> {
        self.send_message(EngineMessage::ClientMessage(self.session_id, msg))
    }

    fn send_message(&self, msg: EngineMessage) -> Result<(), EngineError> {
        Ok(self.cmd_tx.try_send(msg)?)
    }
}

pub struct Engine {
    cmd_rx: Receiver<EngineMessage>,
    log_tx: broadcast::Sender<EngineEvent>,
    perf_tx: watch::Sender<Option<Arc<PerformanceInfo>>>,
    session_seq: Sequence,
    workspace: Workspace,
}

impl Engine {
    fn run(&mut self) {
        let start = Instant::now();
        let mut stat = EngineStat::new();
        let mut tick = 0;

        loop {
            let this_tick = tick;
            tick += 1;

            // we don't simply calculate `tick * TICK_BUDGET` here to prevent loss of precision over time:
            let scheduled_tick_end = start + Duration::from_millis((tick * 1_000) / TICKS_PER_SECOND as u64);

            // run tick
            let indications = stat.record_tick(scheduled_tick_end,
                |tick_stat| self.run_tick(this_tick, tick_stat));

            // send out indication updates
            for (module_id, indication) in indications {
                self.workspace.indications.insert(module_id, indication.clone());
                self.log_op(ServerUpdate::UpdateModuleIndication(module_id, indication));
            }

            // send out performance metrics
            if (this_tick % (TICKS_PER_SECOND as u64 / 2)) == 0 {
                let _ = self.perf_tx.broadcast(Some(Arc::new(stat.report())));
            }

            // process all waiting commands immediately
            loop {
                match self.cmd_rx.try_recv() {
                    Ok(msg) => { self.process_message(msg, &mut stat); }
                    Err(TryRecvError::Empty) => { break; }
                    Err(TryRecvError::Disconnected) => { return; }
                }
            }

            // wait for next tick and process commands while waiting
            loop {
                let now = Instant::now();

                if now >= scheduled_tick_end {
                    break;
                }

                match self.cmd_rx.recv_timeout(scheduled_tick_end - now) {
                    Ok(msg) => { self.process_message(msg, &mut stat); }
                    Err(RecvTimeoutError::Timeout) => { break; }
                    Err(RecvTimeoutError::Disconnected) => { return; }
                }
            }
        }
    }

    fn process_message(&mut self, msg: EngineMessage, stat: &mut EngineStat) {
        match msg {
            EngineMessage::ConnectSession(tx) => {
                let _ = tx.send(self.connect_session());
            }
            EngineMessage::ClientMessage(session, msg) => {
                self.client_update(session, msg, stat);
            }
        }
    }

    fn connect_session(&mut self) -> (SessionId, WorkspaceState, EngineEvents) {
        let session_id = SessionId(self.session_seq.next());
        let log_rx = self.log_tx.subscribe();
        let state = self.dump_state();
        (session_id, state, log_rx)
    }

    fn dump_state(&self) -> WorkspaceState {
        let mut state = WorkspaceState {
            modules: Vec::new(),
            geometry: Vec::new(),
            indications: Vec::new(),
            connections: Vec::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
        };

        for (module_id, module) in &self.workspace.modules {
            state.modules.push((*module_id, module.params()));
            state.inputs.push((*module_id, module.inputs().to_vec()));
            state.outputs.push((*module_id, module.outputs().to_vec()));
        }

        for (module_id, geometry) in &self.workspace.geometry {
            state.geometry.push((*module_id, geometry.clone()));
        }

        for (module_id, indication) in &self.workspace.indications {
            state.indications.push((*module_id, indication.clone()));
        }

        for (input, output) in &self.workspace.connections {
            state.connections.push((*input, *output));
        }

        state
    }

    fn log_op(&mut self, op: ServerUpdate) {
        let _ = self.log_tx.send(EngineEvent::ServerUpdate(op));
    }

    fn sync_log(&mut self, clock: OpClock) {
        let _ = self.log_tx.send(EngineEvent::Sync(clock));
    }

    fn client_update(&mut self, session_id: SessionId, msg: ClientMessage, stat: &mut EngineStat) {
        let clock = OpClock(session_id, msg.sequence);

        match msg.op {
            ClientOp::CreateModule(params, geometry) => {
                // TODO - the audio engine is not actually concerned with
                // window geometry and so should not own this data and force
                // all accesses to it to go via the live audio thread
                let id = ModuleId(self.workspace.module_seq.next());
                let (module, indication) = Module::create(params.clone());
                let inputs = module.inputs().to_vec();
                let outputs = module.outputs().to_vec();
                self.workspace.modules.insert(id, module);
                self.workspace.geometry.insert(id, geometry.clone());
                self.workspace.indications.insert(id, indication.clone());

                self.log_op(ServerUpdate::CreateModule {
                    id,
                    params,
                    geometry,
                    indication,
                    inputs,
                    outputs,
                });
            }
            ClientOp::UpdateModuleParams(module_id, params) => {
                if let Some(module) = self.workspace.modules.get_mut(&module_id) {
                    module.update(params.clone());
                    self.log_op(ServerUpdate::UpdateModuleParams(module_id, params));
                }
            }
            ClientOp::UpdateWindowGeometry(module_id, geometry) => {
                if let Some(geom) = self.workspace.geometry.get_mut(&module_id) {
                    *geom = geometry.clone();
                    self.log_op(ServerUpdate::UpdateWindowGeometry(module_id, geometry));
                }
            }
            ClientOp::DeleteModule(module_id) => {
                // find any connections connected to this module's inputs or
                // outputs and delete them, generating oplog entries

                let mut deleted_connections = Vec::new();

                for (input, output) in &self.workspace.connections {
                    if input.module_id() == module_id || output.module_id() == module_id {
                        deleted_connections.push(*input);
                    }
                }

                for deleted_connection in deleted_connections {
                    self.workspace.connections.remove(&deleted_connection);
                    self.log_op(ServerUpdate::DeleteConnection(deleted_connection));
                }

                // finally, delete the module:

                if self.workspace.modules.contains_key(&module_id) {
                    self.workspace.modules.remove(&module_id);
                    self.log_op(ServerUpdate::DeleteModule(module_id));
                }

                stat.remove_module(module_id);
            }
            ClientOp::CreateConnection(input_id, output_id) => {
                match self.workspace.connect(input_id, output_id) {
                    Ok(old_output) => {
                        if let Some(_) = old_output {
                            self.log_op(ServerUpdate::DeleteConnection(input_id));
                        }

                        self.log_op(ServerUpdate::CreateConnection(input_id, output_id));
                    }
                    Err(_) => {
                        // client should have guarded against a type mismatched
                        // connection, just drop
                    }
                }
            }
            ClientOp::DeleteConnection(input_id) => {
                if let Some(_) = self.workspace.connections.remove(&input_id) {
                    self.log_op(ServerUpdate::DeleteConnection(input_id));
                }
            }
        }

        return self.sync_log(clock);
    }

    fn run_tick(&mut self, tick: u64, stat: &mut TickStat) -> Vec<(ModuleId, Indication)> {
        // find terminal modules - modules which do not send their output to
        // the input of any other module

        let mut terminal_modules = HashSet::new();

        for (id, _) in &self.workspace.modules {
            terminal_modules.insert(*id);
        }

        for (_, output) in &self.workspace.connections {
            terminal_modules.remove(&output.module_id());
        }

        // depth-first-search modules out via their inputs, starting from
        // terminal modules

        let mut topsort = Topsort {
            modules: &self.workspace.modules,
            connections: &self.workspace.connections,
            run_order: Vec::new(),
            seen: HashSet::new(),
        };

        for id in terminal_modules.into_iter() {
            traverse(id, &mut topsort);
        }

        struct Topsort<'a> {
            modules: &'a HashMap<ModuleId, Module>,
            connections: &'a HashMap<InputId, OutputId>,
            run_order: Vec<ModuleId>,
            seen: HashSet<ModuleId>,
        }

        fn traverse(module_id: ModuleId, state: &mut Topsort) {
            if state.seen.contains(&module_id) {
                return;
            }

            state.seen.insert(module_id);

            let module = &state.modules[&module_id];

            for i in 0..module.inputs().len() {
                let terminal_id = InputId(module_id, i);

                if let Some(output_id) = state.connections.get(&terminal_id) {
                    traverse(output_id.module_id(), state);
                }
            }

            state.run_order.push(module_id);
        }

        // run modules in dependency order according to BFS above

        let mut buffers = HashMap::<OutputId, Output>::new();
        let mut indications = Vec::new();

        for module_id in topsort.run_order.iter() {
            let module = self.workspace.modules.get_mut(&module_id)
                .expect("module get_mut");

            let connections = &self.workspace.connections;

            let mut output_buffers = module.outputs().iter()
                .map(|output| Output::from_line_type(output.line_type()))
                .collect::<Vec<_>>();

            {
                let input_refs = module.inputs().iter()
                    .enumerate()
                    .map(|(i, _ty)| InputId(*module_id, i))
                    .map(|input_id| {
                        connections.get(&input_id)
                            .and_then(|output_id| buffers.get(output_id))
                            .map(|output| output.as_input_ref())
                            .unwrap_or(InputRef::Disconnected)
                    })
                    .collect::<Vec<_>>();

                let mut output_refs = output_buffers.iter_mut()
                    .map(|output| output.as_output_ref())
                    .collect::<Vec<_>>();

                let t = tick * SAMPLES_PER_TICK as u64;

                let result = stat.record_module(*module_id, || {
                    module.run_tick(t, &input_refs, &mut output_refs)
                });

                match result {
                    None => {}
                    Some(indic) => {
                        indications.push((*module_id, indic));
                    }
                }
            }

            for (i, output) in output_buffers.into_iter().enumerate() {
                buffers.insert(OutputId(*module_id, i), output);
            }
        }

        indications
    }
}

pub struct Workspace {
    module_seq: Sequence,
    modules: HashMap<ModuleId, Module>,
    geometry: HashMap<ModuleId, WindowGeometry>,
    connections: HashMap<InputId, OutputId>,
    indications: HashMap<ModuleId, Indication>,
}

impl Workspace {
    pub fn new() -> Self {
        Workspace {
            module_seq: Sequence::new(),
            modules: HashMap::new(),
            geometry: HashMap::new(),
            connections: HashMap::new(),
            indications: HashMap::new(),
        }
    }

    pub fn load(save: &save::Workspace) -> Self {
        let mut modules = HashMap::new();
        let mut geometry = HashMap::new();
        let mut indications = HashMap::new();

        // load modules and geometry
        for (module_id, saved_module) in &save.modules {
            let (module, indication) = Module::create(saved_module.params.clone());
            modules.insert(*module_id, module);
            geometry.insert(*module_id, saved_module.geometry.clone());
            indications.insert(*module_id, indication);
        }

        let mut workspace = Workspace {
            module_seq: save.module_seq.clone(),
            modules,
            geometry,
            connections: HashMap::new(),
            indications,
        };

        // load connections after loading all modules
        for (module_id, saved_module) in &save.modules {
            for (input_idx, output_id) in saved_module.inputs.iter().enumerate() {
                let input_id = InputId(*module_id, input_idx);

                if let Some(output_id) = output_id {
                    // ignore workspace connect error for now... should we log?
                    let _ = workspace.connect(input_id, *output_id);
                }
            }
        }

        workspace
    }

    pub fn save(&self) -> save::Workspace {
        save::Workspace {
            module_seq: self.module_seq.clone(),
            modules: self.modules.iter()
                .map(|(module_id, module)| {
                    let params = module.params();

                    let geometry = self.geometry.get(&module_id)
                        .cloned()
                        .unwrap_or_default();

                    let inputs = (0..module.inputs().len())
                        .map(|idx| InputId(*module_id, idx))
                        .map(|input_id| self.connections.get(&input_id).cloned())
                        .collect();

                    (*module_id, save::Module {
                        params,
                        geometry,
                        inputs,
                    })
                })
                .collect()
        }
    }

    fn terminal_type(&self, terminal: TerminalId) -> Option<LineType> {
        self.modules.get(&terminal.module_id()).and_then(|module| {
            match terminal {
                TerminalId::Input(input) => {
                    module.inputs().get(input.index()).map(|terminal| terminal.line_type())
                }
                TerminalId::Output(output) => {
                    module.outputs().get(output.index()).map(|terminal| terminal.line_type())
                }
            }
        })
    }

    pub fn connect(&mut self, input_id: InputId, output_id: OutputId) -> Result<Option<OutputId>, ConnectError> {
        let input_type = match self.terminal_type(TerminalId::Input(input_id)) {
            Some(ty) => ty,
            None => return Err(ConnectError::NoInput),
        };

        let output_type = match self.terminal_type(TerminalId::Output(output_id)) {
            Some(ty) => ty,
            None => return Err(ConnectError::NoOutput),
        };

        if input_type == output_type {
            Ok(self.connections.insert(input_id, output_id))
        } else {
            // type mismatch, don't connect
            Err(ConnectError::TypeMismatch)
        }
    }
}

pub enum ConnectError {
    NoInput,
    NoOutput,
    TypeMismatch,
}
