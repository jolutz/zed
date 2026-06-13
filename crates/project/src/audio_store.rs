use crate::{
    Project, ProjectEntryId, ProjectItem, ProjectPath,
    worktree_store::{WorktreeStore, WorktreeStoreEvent},
};
use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet, hash_map};
use futures::{StreamExt, channel::oneshot};
use gpui::{App, Context, Entity, EventEmitter, Subscription, Task, WeakEntity, prelude::*};
use language::{DiskState, File};
use rpc::{AnyProtoClient, TypedEnvelope, proto};
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::Arc;
use util::{ResultExt, rel_path::RelPath};
use worktree::{LoadedBinaryFile, PathChange, Worktree, WorktreeId};

const AUDIO_EXTENSIONS: &[&str] = &["wav", "mp3", "flac", "ogg"];

#[derive(Clone, Copy, Debug, Hash, PartialEq, PartialOrd, Ord, Eq)]
pub struct AudioId(NonZeroU64);

impl AudioId {
    pub fn to_proto(&self) -> u64 {
        self.0.get()
    }
}

impl From<NonZeroU64> for AudioId {
    fn from(id: NonZeroU64) -> Self {
        AudioId(id)
    }
}

#[derive(Debug)]
pub enum AudioItemEvent {
    ReloadNeeded,
    Reloaded,
    FileHandleChanged,
}

impl EventEmitter<AudioItemEvent> for AudioItem {}

pub enum AudioStoreEvent {
    AudioAdded(Entity<AudioItem>),
}

impl EventEmitter<AudioStoreEvent> for AudioStore {}

pub struct AudioItem {
    pub id: AudioId,
    pub file: Arc<worktree::File>,
    pub bytes: Arc<Vec<u8>>,
    reload_task: Option<Task<()>>,
}

impl AudioItem {
    pub fn new(id: AudioId, file: Arc<worktree::File>, bytes: Vec<u8>) -> Self {
        Self {
            id,
            file,
            bytes: Arc::new(bytes),
            reload_task: None,
        }
    }

    pub fn project_path(&self, cx: &App) -> ProjectPath {
        ProjectPath {
            worktree_id: self.file.worktree_id(cx),
            path: self.file.path().clone(),
        }
    }

    pub fn abs_path(&self, cx: &App) -> Option<PathBuf> {
        Some(self.file.as_local()?.abs_path(cx))
    }

    fn file_updated(&mut self, new_file: Arc<worktree::File>, cx: &mut Context<Self>) {
        let mut file_changed = false;

        let old_file = &self.file;
        if new_file.path() != old_file.path() {
            file_changed = true;
        }

        let old_state = old_file.disk_state();
        let new_state = new_file.disk_state();
        if old_state != new_state {
            file_changed = true;
            if matches!(new_state, DiskState::Present { .. }) {
                cx.emit(AudioItemEvent::ReloadNeeded);
                self.reload(cx);
            }
        }

        self.file = new_file;
        if file_changed {
            cx.emit(AudioItemEvent::FileHandleChanged);
            cx.notify();
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) -> Option<oneshot::Receiver<()>> {
        let local_file = self.file.as_local()?;
        let (tx, rx) = oneshot::channel();

        let content = local_file.load_bytes(cx);
        self.reload_task = Some(cx.spawn(async move |this, cx| {
            if let Some(bytes) = content
                .await
                .context("Failed to load audio content")
                .log_err()
            {
                this.update(cx, |this, cx| {
                    this.bytes = Arc::new(bytes);
                    cx.emit(AudioItemEvent::Reloaded);
                })
                .log_err();
            }
            tx.send(()).ok();
        }));
        Some(rx)
    }
}

pub fn is_audio_file(project: &Entity<Project>, path: &ProjectPath, cx: &App) -> bool {
    let extension = util::maybe!({
        let worktree_abs_path = project
            .read(cx)
            .worktree_for_id(path.worktree_id, cx)?
            .read(cx)
            .abs_path();
        path.path
            .extension()
            .or_else(|| worktree_abs_path.extension()?.to_str())
            .map(str::to_lowercase)
    });

    extension
        .as_deref()
        .is_some_and(|extension| AUDIO_EXTENSIONS.contains(&extension))
}

impl ProjectItem for AudioItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        if is_audio_file(project, path, cx) {
            Some(cx.spawn({
                let path = path.clone();
                let project = project.clone();
                async move |cx| {
                    project
                        .update(cx, |project, cx| project.open_audio(path, cx))
                        .await
                }
            }))
        } else {
            None
        }
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        self.file.entry_id
    }

    fn project_path(&self, cx: &App) -> Option<ProjectPath> {
        Some(self.project_path(cx))
    }

    fn is_dirty(&self) -> bool {
        false
    }
}

trait AudioStoreImpl {
    fn open_audio(
        &self,
        path: Arc<RelPath>,
        worktree: Entity<Worktree>,
        cx: &mut Context<AudioStore>,
    ) -> Task<Result<Entity<AudioItem>>>;

    fn reload_audios(
        &self,
        audios: HashSet<Entity<AudioItem>>,
        cx: &mut Context<AudioStore>,
    ) -> Task<Result<()>>;

    fn as_local(&self) -> Option<Entity<LocalAudioStore>>;
    fn as_remote(&self) -> Option<Entity<RemoteAudioStore>>;
}

struct RemoteAudioStore {
    upstream_client: AnyProtoClient,
    project_id: u64,
    loading_remote_audios_by_id: HashMap<AudioId, LoadingRemoteAudio>,
    remote_audio_listeners: HashMap<AudioId, Vec<oneshot::Sender<Result<Entity<AudioItem>>>>>,
    loaded_audios: HashMap<AudioId, Entity<AudioItem>>,
}

struct LoadingRemoteAudio {
    state: proto::AudioState,
    chunks: Vec<Vec<u8>>,
    received_size: u64,
}

struct LocalAudioStore {
    next_audio_id: u64,
    local_audio_ids_by_path: HashMap<ProjectPath, AudioId>,
    local_audio_ids_by_entry_id: HashMap<ProjectEntryId, AudioId>,
    audio_store: WeakEntity<AudioStore>,
    _subscription: Subscription,
}

pub struct AudioStore {
    state: Box<dyn AudioStoreImpl>,
    opened_audios: HashMap<AudioId, WeakEntity<AudioItem>>,
    worktree_store: Entity<WorktreeStore>,
    #[allow(clippy::type_complexity)]
    loading_audios_by_path: HashMap<
        ProjectPath,
        postage::watch::Receiver<Option<Result<Entity<AudioItem>, Arc<anyhow::Error>>>>,
    >,
}

impl AudioStore {
    pub fn local(worktree_store: Entity<WorktreeStore>, cx: &mut Context<Self>) -> Self {
        let this = cx.weak_entity();
        Self {
            state: Box::new(cx.new(|cx| {
                let subscription = cx.subscribe(
                    &worktree_store,
                    |this: &mut LocalAudioStore, _, event, cx| {
                        if let WorktreeStoreEvent::WorktreeAdded(worktree) = event {
                            this.subscribe_to_worktree(worktree, cx);
                        }
                    },
                );

                LocalAudioStore {
                    next_audio_id: 1,
                    local_audio_ids_by_path: Default::default(),
                    local_audio_ids_by_entry_id: Default::default(),
                    audio_store: this,
                    _subscription: subscription,
                }
            })),
            opened_audios: Default::default(),
            loading_audios_by_path: Default::default(),
            worktree_store,
        }
    }

    pub fn remote(
        worktree_store: Entity<WorktreeStore>,
        upstream_client: AnyProtoClient,
        project_id: u64,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            state: Box::new(cx.new(|_| RemoteAudioStore {
                upstream_client,
                project_id,
                loading_remote_audios_by_id: Default::default(),
                remote_audio_listeners: Default::default(),
                loaded_audios: Default::default(),
            })),
            opened_audios: Default::default(),
            loading_audios_by_path: Default::default(),
            worktree_store,
        }
    }

    pub fn audios(&self) -> impl '_ + Iterator<Item = Entity<AudioItem>> {
        self.opened_audios
            .values()
            .filter_map(|audio| audio.upgrade())
    }

    pub fn get(&self, audio_id: AudioId) -> Option<Entity<AudioItem>> {
        self.opened_audios
            .get(&audio_id)
            .and_then(|audio| audio.upgrade())
    }

    pub fn get_by_path(&self, path: &ProjectPath, cx: &App) -> Option<Entity<AudioItem>> {
        self.audios()
            .find(|audio| &audio.read(cx).project_path(cx) == path)
    }

    pub fn open_audio(
        &mut self,
        project_path: ProjectPath,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<AudioItem>>> {
        if let Some(existing_audio) = self.get_by_path(&project_path, cx) {
            return Task::ready(Ok(existing_audio));
        }

        let Some(worktree) = self
            .worktree_store
            .read(cx)
            .worktree_for_id(project_path.worktree_id, cx)
        else {
            return Task::ready(Err(anyhow::anyhow!("no such worktree")));
        };

        let loading_watch = match self.loading_audios_by_path.entry(project_path.clone()) {
            hash_map::Entry::Occupied(entry) => entry.get().clone(),
            hash_map::Entry::Vacant(entry) => {
                let (mut tx, rx) = postage::watch::channel();
                entry.insert(rx.clone());

                let load_audio = self
                    .state
                    .open_audio(project_path.path.clone(), worktree, cx);

                cx.spawn(async move |this, cx| {
                    let load_result = load_audio.await;
                    *tx.borrow_mut() = Some(this.update(cx, |this, _cx| {
                        this.loading_audios_by_path.remove(&project_path);
                        let audio = load_result.map_err(Arc::new)?;
                        Ok(audio)
                    })?);
                    anyhow::Ok(())
                })
                .detach();
                rx
            }
        };

        cx.spawn(async move |_this, _cx| {
            let mut loading_watch = loading_watch.clone();
            loop {
                if let Some(result) = loading_watch.borrow().clone() {
                    return result.map_err(|error| anyhow::anyhow!("{error:?}"));
                }
                loading_watch.next().await;
            }
        })
    }

    pub fn reload_audios(
        &mut self,
        audios: HashSet<Entity<AudioItem>>,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.state.reload_audios(audios, cx)
    }

    fn add_audio(&mut self, audio: Entity<AudioItem>, cx: &mut Context<AudioStore>) -> Result<()> {
        let audio_id = audio.read(cx).id;
        let old_audio = self.opened_audios.insert(audio_id, audio.downgrade());
        if old_audio.is_some_and(|old_audio| old_audio.upgrade().is_some()) {
            anyhow::bail!("Audio already exists");
        }

        cx.subscribe(&audio, Self::on_audio_event).detach();
        cx.emit(AudioStoreEvent::AudioAdded(audio));
        Ok(())
    }

    fn on_audio_event(
        &mut self,
        audio: Entity<AudioItem>,
        event: &AudioItemEvent,
        cx: &mut Context<Self>,
    ) {
        if let AudioItemEvent::FileHandleChanged = event
            && let Some(local) = self.state.as_local()
        {
            local.update(cx, |local, cx| {
                local.audio_changed_file(audio, cx);
            })
        }
    }

    pub fn handle_create_audio_for_peer(
        &mut self,
        envelope: TypedEnvelope<proto::CreateAudioForPeer>,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        if let Some(remote) = self.state.as_remote() {
            let worktree_store = self.worktree_store.clone();
            let audio = remote.update(cx, |remote, cx| {
                remote.handle_create_audio_for_peer(envelope, &worktree_store, cx)
            })?;
            if let Some(audio) = audio {
                remote.update(cx, |this, cx| {
                    let audio = audio.clone();
                    let audio_id = audio.read(cx).id;
                    this.loaded_audios.insert(audio_id, audio)
                });

                self.add_audio(audio, cx)?;
            }
        }

        Ok(())
    }
}

impl RemoteAudioStore {
    pub fn wait_for_remote_audio(
        &mut self,
        id: AudioId,
        cx: &mut Context<Self>,
    ) -> Task<Result<Entity<AudioItem>>> {
        if let Some(audio) = self.loaded_audios.remove(&id) {
            return Task::ready(Ok(audio));
        }

        let (tx, rx) = oneshot::channel();
        self.remote_audio_listeners.entry(id).or_default().push(tx);

        cx.spawn(async move |_this, cx| cx.background_spawn(async move { rx.await? }).await)
    }

    pub fn handle_create_audio_for_peer(
        &mut self,
        envelope: TypedEnvelope<proto::CreateAudioForPeer>,
        worktree_store: &Entity<WorktreeStore>,
        cx: &mut Context<Self>,
    ) -> Result<Option<Entity<AudioItem>>> {
        use proto::create_audio_for_peer::Variant;
        match envelope.payload.variant {
            Some(Variant::State(state)) => {
                let audio_id =
                    AudioId::from(NonZeroU64::new(state.id).context("invalid audio id")?);

                self.loading_remote_audios_by_id.insert(
                    audio_id,
                    LoadingRemoteAudio {
                        state,
                        chunks: Vec::new(),
                        received_size: 0,
                    },
                );
                Ok(None)
            }
            Some(Variant::Chunk(chunk)) => {
                let audio_id =
                    AudioId::from(NonZeroU64::new(chunk.audio_id).context("invalid audio id")?);

                let loading = self
                    .loading_remote_audios_by_id
                    .get_mut(&audio_id)
                    .context("received chunk for unknown audio")?;

                loading.received_size += chunk.data.len() as u64;
                loading.chunks.push(chunk.data);

                if loading.received_size == loading.state.content_size {
                    let loading = self.loading_remote_audios_by_id.remove(&audio_id).unwrap();

                    let mut content = Vec::with_capacity(loading.received_size as usize);
                    for chunk_data in loading.chunks {
                        content.extend_from_slice(&chunk_data);
                    }

                    let proto_file = loading.state.file.context("missing file in audio state")?;
                    let worktree_id = WorktreeId::from_proto(proto_file.worktree_id);
                    let worktree = worktree_store
                        .read(cx)
                        .worktree_for_id(worktree_id, cx)
                        .context("worktree not found")?;

                    let file = Arc::new(
                        worktree::File::from_proto(proto_file, worktree, cx)
                            .context("invalid file in audio state")?,
                    );

                    let entity = cx.new(|_cx| AudioItem::new(audio_id, file, content));

                    if let Some(listeners) = self.remote_audio_listeners.remove(&audio_id) {
                        for listener in listeners {
                            listener.send(Ok(entity.clone())).ok();
                        }
                    }

                    Ok(Some(entity))
                } else {
                    Ok(None)
                }
            }
            None => {
                log::warn!("Received CreateAudioForPeer with no variant");
                Ok(None)
            }
        }
    }
}

impl AudioStoreImpl for Entity<LocalAudioStore> {
    fn open_audio(
        &self,
        path: Arc<RelPath>,
        worktree: Entity<Worktree>,
        cx: &mut Context<AudioStore>,
    ) -> Task<Result<Entity<AudioItem>>> {
        let this = self.clone();

        let load_file = worktree.update(cx, |worktree, cx| {
            worktree.load_binary_file(path.as_ref(), cx)
        });

        cx.spawn(async move |audio_store, cx| {
            let LoadedBinaryFile { file, content } = load_file.await?;
            let audio_id = this.update(cx, |this, _cx| {
                let id = NonZeroU64::new(this.next_audio_id).context("invalid audio id")?;
                this.next_audio_id += 1;
                anyhow::Ok(AudioId::from(id))
            })?;
            let entity = cx.new(|_cx| AudioItem::new(audio_id, file, content));

            audio_store.update(cx, |audio_store, cx| {
                audio_store.add_audio(entity.clone(), cx)
            })??;

            Ok(entity)
        })
    }

    fn reload_audios(
        &self,
        audios: HashSet<Entity<AudioItem>>,
        cx: &mut Context<AudioStore>,
    ) -> Task<Result<()>> {
        cx.spawn(async move |_audio_store, cx| {
            let reloads = audios
                .into_iter()
                .filter_map(|audio| audio.update(cx, |audio, cx| audio.reload(cx)))
                .collect::<Vec<_>>();
            for reload in reloads {
                reload.await.ok();
            }
            Ok(())
        })
    }

    fn as_local(&self) -> Option<Entity<LocalAudioStore>> {
        Some(self.clone())
    }

    fn as_remote(&self) -> Option<Entity<RemoteAudioStore>> {
        None
    }
}

impl AudioStoreImpl for Entity<RemoteAudioStore> {
    fn open_audio(
        &self,
        path: Arc<RelPath>,
        worktree: Entity<Worktree>,
        cx: &mut Context<AudioStore>,
    ) -> Task<Result<Entity<AudioItem>>> {
        let worktree_id = worktree.read(cx).id().to_proto();
        let (project_id, client) = {
            let store = self.read(cx);
            (store.project_id, store.upstream_client.clone())
        };
        let remote_store = self.clone();

        cx.spawn(async move |_audio_store, cx| {
            let response = client
                .request(proto::OpenAudioByPath {
                    project_id,
                    worktree_id,
                    path: path.to_proto(),
                })
                .await?;

            let audio_id = AudioId::from(
                NonZeroU64::new(response.audio_id).context("invalid audio_id in response")?,
            );

            remote_store
                .update(cx, |remote_store, cx| {
                    remote_store.wait_for_remote_audio(audio_id, cx)
                })
                .await
        })
    }

    fn reload_audios(
        &self,
        _audios: HashSet<Entity<AudioItem>>,
        _cx: &mut Context<AudioStore>,
    ) -> Task<Result<()>> {
        Task::ready(Err(anyhow::anyhow!(
            "Reloading audio from remote is not supported"
        )))
    }

    fn as_local(&self) -> Option<Entity<LocalAudioStore>> {
        None
    }

    fn as_remote(&self) -> Option<Entity<RemoteAudioStore>> {
        Some(self.clone())
    }
}

impl LocalAudioStore {
    fn subscribe_to_worktree(&mut self, worktree: &Entity<Worktree>, cx: &mut Context<Self>) {
        cx.subscribe(worktree, |this, worktree, event, cx| {
            if worktree.read(cx).is_local()
                && let worktree::Event::UpdatedEntries(changes) = event
            {
                this.local_worktree_entries_changed(&worktree, changes, cx);
            }
        })
        .detach();
    }

    fn local_worktree_entries_changed(
        &mut self,
        worktree_handle: &Entity<Worktree>,
        changes: &[(Arc<RelPath>, ProjectEntryId, PathChange)],
        cx: &mut Context<Self>,
    ) {
        let snapshot = worktree_handle.read(cx).snapshot();
        for (path, entry_id, _) in changes {
            self.local_worktree_entry_changed(*entry_id, path, worktree_handle, &snapshot, cx);
        }
    }

    fn local_worktree_entry_changed(
        &mut self,
        entry_id: ProjectEntryId,
        path: &Arc<RelPath>,
        worktree: &Entity<Worktree>,
        snapshot: &worktree::Snapshot,
        cx: &mut Context<Self>,
    ) -> Option<()> {
        let project_path = ProjectPath {
            worktree_id: snapshot.id(),
            path: path.clone(),
        };
        let audio_id = match self.local_audio_ids_by_entry_id.get(&entry_id) {
            Some(&audio_id) => audio_id,
            None => self.local_audio_ids_by_path.get(&project_path).copied()?,
        };

        let audio = self
            .audio_store
            .update(cx, |audio_store, _| {
                if let Some(audio) = audio_store.get(audio_id) {
                    Some(audio)
                } else {
                    audio_store.opened_audios.remove(&audio_id);
                    None
                }
            })
            .ok()
            .flatten();
        let audio = if let Some(audio) = audio {
            audio
        } else {
            self.local_audio_ids_by_path.remove(&project_path);
            self.local_audio_ids_by_entry_id.remove(&entry_id);
            return None;
        };

        audio.update(cx, |audio, cx| {
            let old_file = &audio.file;
            if old_file.worktree != *worktree {
                return;
            }

            let snapshot_entry = old_file
                .entry_id
                .and_then(|entry_id| snapshot.entry_for_id(entry_id))
                .or_else(|| snapshot.entry_for_path(old_file.path.as_ref()));

            let new_file = if let Some(entry) = snapshot_entry {
                worktree::File {
                    disk_state: match entry.mtime {
                        Some(mtime) => DiskState::Present {
                            mtime,
                            size: entry.size,
                        },
                        None => old_file.disk_state,
                    },
                    is_local: true,
                    entry_id: Some(entry.id),
                    path: entry.path.clone(),
                    worktree: worktree.clone(),
                    is_private: entry.is_private,
                }
            } else {
                worktree::File {
                    disk_state: DiskState::Deleted,
                    is_local: true,
                    entry_id: old_file.entry_id,
                    path: old_file.path.clone(),
                    worktree: worktree.clone(),
                    is_private: old_file.is_private,
                }
            };

            if new_file == **old_file {
                return;
            }

            if new_file.path != old_file.path {
                self.local_audio_ids_by_path.remove(&ProjectPath {
                    path: old_file.path.clone(),
                    worktree_id: old_file.worktree_id(cx),
                });
                self.local_audio_ids_by_path.insert(
                    ProjectPath {
                        worktree_id: new_file.worktree_id(cx),
                        path: new_file.path.clone(),
                    },
                    audio_id,
                );
            }

            if new_file.entry_id != old_file.entry_id {
                if let Some(entry_id) = old_file.entry_id {
                    self.local_audio_ids_by_entry_id.remove(&entry_id);
                }
                if let Some(entry_id) = new_file.entry_id {
                    self.local_audio_ids_by_entry_id.insert(entry_id, audio_id);
                }
            }

            audio.file_updated(Arc::new(new_file), cx);
        });
        None
    }

    fn audio_changed_file(&mut self, audio: Entity<AudioItem>, cx: &mut App) -> Option<()> {
        let audio = audio.read(cx);
        let file = &audio.file;

        let audio_id = audio.id;
        if let Some(entry_id) = file.entry_id {
            if self.local_audio_ids_by_entry_id.contains_key(&entry_id) {
                return None;
            }
            self.local_audio_ids_by_entry_id.insert(entry_id, audio_id);
        };
        self.local_audio_ids_by_path.insert(
            ProjectPath {
                worktree_id: file.worktree_id(cx),
                path: file.path.clone(),
            },
            audio_id,
        );

        Some(())
    }
}
