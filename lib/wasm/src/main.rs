// use wasm_bindgen::prelude::*;

use std::collections::HashMap;
// use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
// use std::sync::Once;

use itertools::Itertools;
use jj_lib::backend;
use jj_lib::backend::Backend;
use jj_lib::backend::BackendInitError;
use jj_lib::backend::ChangeId;
use jj_lib::backend::CommitId;
use jj_lib::backend::FileId;
use jj_lib::backend::MergedTreeId;
use jj_lib::backend::MillisSinceEpoch;
use jj_lib::backend::Signature;
use jj_lib::backend::Timestamp;
use jj_lib::backend::TreeValue;
use jj_lib::commit::Commit;
use jj_lib::commit_builder::CommitBuilder;
use jj_lib::config::ConfigLayer;
use jj_lib::config::ConfigSource;
use jj_lib::config::StackedConfig;
use jj_lib::local_backend::LocalBackend;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::MutableRepo;
use jj_lib::repo::ReadonlyRepo;
use jj_lib::repo::Repo;
use jj_lib::repo::RepoLoader;
use jj_lib::repo::StoreFactories;
use jj_lib::repo_path::RepoPath;
use jj_lib::repo_path::RepoPathBuf;
// use jj_lib::secret_backend::SecretBackend;
use jj_lib::op_store::WorkspaceId;
use jj_lib::settings::UserSettings;
use jj_lib::signing::Signer;
use jj_lib::store::Store;
use jj_lib::transaction::Transaction;
use jj_lib::tree::Tree;
use jj_lib::tree_builder::TreeBuilder;
use jj_lib::working_copy::SnapshotError;
use jj_lib::working_copy::SnapshotOptions;
use jj_lib::working_copy::SnapshotStats;
use jj_lib::workspace::Workspace;

use pollster::FutureExt;
// use tempfile::TempDir;

// use crate::test_backend::TestBackendFactory;
mod test_backend;
use test_backend::TestBackendFactory;

pub fn new_temp_dir() -> TempDir {
    // Create a unique subdirectory in /tmp
    let mut path = std::path::PathBuf::from("/tmp");

    // Generate a unique directory name
    let unique_dir = format!(
        "jj-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );

    path.push(unique_dir);

    // Create the directory
    std::fs::create_dir_all(&path).expect("Failed to create temp directory");

    TempDir::from_path(path)
}

#[derive(Debug)]
pub struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    fn from_path(path: std::path::PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // Clean up the temporary directory when the TempDir is dropped
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Returns new low-level config object that includes fake user configuration
/// needed to run basic operations.
pub fn base_user_config() -> StackedConfig {
    let config_text = r#"
        user.name = "Test User"
        user.email = "test.user@example.com"
        operation.username = "test-username"
        operation.hostname = "host.example.com"
        debug.randomness-seed = 42
    "#;
    let mut config = StackedConfig::with_defaults();
    config.add_layer(ConfigLayer::parse(ConfigSource::User, config_text).unwrap());
    config
}

/// Returns new immutable settings object that includes fake user configuration
/// needed to run basic operations.
pub fn user_settings() -> UserSettings {
    UserSettings::from_config(base_user_config()).unwrap()
}

#[derive(Debug)]
pub struct TestEnvironment {
    temp_dir: TempDir,
    test_backend_factory: TestBackendFactory,
}

impl TestEnvironment {
    pub fn init() -> Self {
        TestEnvironment {
            temp_dir: new_temp_dir(),
            test_backend_factory: TestBackendFactory::default(),
        }
    }

    pub fn root(&self) -> &Path {
        self.temp_dir.path()
    }

    pub fn default_store_factories(&self) -> StoreFactories {
        let mut factories = StoreFactories::default();
        factories.add_backend("test", {
            let factory = self.test_backend_factory.clone();
            Box::new(move |_settings, store_path| Ok(Box::new(factory.load(store_path))))
        });
        factories
    }

    pub fn load_repo_at_head(
        &self,
        settings: &UserSettings,
        repo_path: &Path,
    ) -> Arc<ReadonlyRepo> {
        RepoLoader::init_from_file_system(settings, repo_path, &self.default_store_factories())
            .unwrap()
            .load_at_head()
            .unwrap()
    }
}

pub struct TestRepo {
    pub env: TestEnvironment,
    pub repo: Arc<ReadonlyRepo>,
    repo_path: PathBuf,
}

#[derive(PartialEq, Eq, Copy, Clone)]
pub enum TestRepoBackend {
    Local,
    Test,
}

impl TestRepoBackend {
    fn init_backend(
        &self,
        env: &TestEnvironment,
        settings: &UserSettings,
        store_path: &Path,
    ) -> Result<Box<dyn Backend>, BackendInitError> {
        match self {
            TestRepoBackend::Local => Ok(Box::new(LocalBackend::init(store_path))),
            TestRepoBackend::Test => Ok(Box::new(env.test_backend_factory.init(store_path))),
        }
    }
}

impl TestRepo {
    pub fn init() -> Self {
        Self::init_with_backend(TestRepoBackend::Test)
    }

    pub fn init_with_backend(backend: TestRepoBackend) -> Self {
        Self::init_with_backend_and_settings(backend, &user_settings())
    }

    pub fn init_with_settings(settings: &UserSettings) -> Self {
        Self::init_with_backend_and_settings(TestRepoBackend::Test, settings)
    }

    pub fn init_with_backend_and_settings(
        backend: TestRepoBackend,
        settings: &UserSettings,
    ) -> Self {
        let env = TestEnvironment::init();

        let repo_dir = env.root().join("repo");

        match fs::create_dir(&repo_dir) {
            Ok(_) => println!("Successfully created directory"),
            Err(e) => println!("Error creating directory: {}", e),
        }

        let repo = match ReadonlyRepo::init(
            settings,
            &repo_dir,
            &|settings, store_path| backend.init_backend(&env, settings, store_path),
            Signer::from_settings(settings).unwrap(),
            ReadonlyRepo::default_op_store_initializer(),
            ReadonlyRepo::default_op_heads_store_initializer(),
            ReadonlyRepo::default_index_store_initializer(),
            ReadonlyRepo::default_submodule_store_initializer(),
        ) {
            Ok(repo) => repo,
            Err(e) => {
                println!("Error initializing ReadonlyRepo: {:?}", e);
                panic!("Failed to initialize repo: {:?}", e);
            }
        };

        Self {
            env,
            repo,
            repo_path: repo_dir,
        }
    }
}

pub struct TestWorkspace {
    pub env: TestEnvironment,
    pub workspace: Workspace,
    pub repo: Arc<ReadonlyRepo>,
}

impl TestWorkspace {
    pub fn init(settings: &UserSettings) -> Self {
        Self::init_with_backend(settings, TestRepoBackend::Test)
    }

    pub fn init_with_backend(settings: &UserSettings, backend: TestRepoBackend) -> Self {
        Self::init_with_backend_and_signer(
            settings,
            backend,
            Signer::from_settings(settings).unwrap(),
        )
    }

    pub fn init_with_backend_and_signer(
        settings: &UserSettings,
        backend: TestRepoBackend,
        signer: Signer,
    ) -> Self {
        let env = TestEnvironment::init();

        let workspace_root = env.root().join("repo");
        fs::create_dir(&workspace_root).unwrap();

        let (workspace, repo) = Workspace::init_with_backend(
            settings,
            &workspace_root,
            &|settings, store_path| backend.init_backend(&env, settings, store_path),
            signer,
        )
        .unwrap();

        Self {
            env,
            workspace,
            repo,
        }
    }

    pub fn root_dir(&self) -> PathBuf {
        self.env.root().join("repo").join("..")
    }

    pub fn repo_path(&self) -> &Path {
        self.workspace.repo_path()
    }

    /// Snapshots the working copy and returns the tree. Updates the working
    /// copy state on disk, but does not update the working-copy commit (no
    /// new operation).
    pub fn snapshot_with_options(
        &mut self,
        options: &SnapshotOptions,
    ) -> Result<(MergedTree, SnapshotStats), SnapshotError> {
        let mut locked_ws = self.workspace.start_working_copy_mutation().unwrap();
        let (tree_id, stats) = locked_ws.locked_wc().snapshot(options)?;
        // arbitrary operation id
        locked_ws.finish(self.repo.op_id().clone()).unwrap();
        Ok((self.repo.store().get_root_tree(&tree_id).unwrap(), stats))
    }

    /// Like `snapshot_with_option()` but with default options
    pub fn snapshot(&mut self) -> Result<MergedTree, SnapshotError> {
        let (tree_id, _stats) = self.snapshot_with_options(&SnapshotOptions::empty_for_test())?;
        Ok(tree_id)
    }
}

pub fn commit_transactions(txs: Vec<Transaction>) -> Arc<ReadonlyRepo> {
    let repo_loader = txs[0].base_repo().loader().clone();
    let mut op_ids = vec![];
    for tx in txs {
        op_ids.push(tx.commit("test").unwrap().op_id().clone());
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let repo = repo_loader.load_at_head().unwrap();
    // Test the setup. The assumption here is that the parent order matches the
    // order in which they were merged (which currently matches the transaction
    // commit order), so we want to know make sure they appear in a certain
    // order, so the caller can decide the order by passing them to this
    // function in a certain order.
    assert_eq!(*repo.operation().parent_ids(), op_ids);
    repo
}

pub fn read_file(store: &Store, path: &RepoPath, id: &FileId) -> Vec<u8> {
    let mut reader = store.read_file(path, id).unwrap();
    let mut content = vec![];
    reader.read_to_end(&mut content).unwrap();
    content
}

pub fn write_file(store: &Store, path: &RepoPath, contents: &str) -> FileId {
    store
        .write_file(path, &mut contents.as_bytes())
        .block_on()
        .unwrap()
}

pub fn write_normal_file(
    tree_builder: &mut TreeBuilder,
    path: &RepoPath,
    contents: &str,
) -> FileId {
    let id = write_file(tree_builder.store(), path, contents);
    tree_builder.set(
        path.to_owned(),
        TreeValue::File {
            id: id.clone(),
            executable: false,
        },
    );
    id
}

pub fn write_executable_file(tree_builder: &mut TreeBuilder, path: &RepoPath, contents: &str) {
    let id = write_file(tree_builder.store(), path, contents);
    tree_builder.set(
        path.to_owned(),
        TreeValue::File {
            id,
            executable: true,
        },
    );
}

pub fn write_symlink(tree_builder: &mut TreeBuilder, path: &RepoPath, target: &str) {
    let id = tree_builder
        .store()
        .write_symlink(path, target)
        .block_on()
        .unwrap();
    tree_builder.set(path.to_owned(), TreeValue::Symlink(id));
}

pub fn create_single_tree(repo: &Arc<ReadonlyRepo>, path_contents: &[(&RepoPath, &str)]) -> Tree {
    let store = repo.store();
    let mut tree_builder = store.tree_builder(store.empty_tree_id().clone());
    for (path, contents) in path_contents {
        write_normal_file(&mut tree_builder, path, contents);
    }
    let id = tree_builder.write_tree().unwrap();
    store.get_tree(RepoPathBuf::root(), &id).unwrap()
}

pub fn create_tree(repo: &Arc<ReadonlyRepo>, path_contents: &[(&RepoPath, &str)]) -> MergedTree {
    MergedTree::resolved(create_single_tree(repo, path_contents))
}

#[must_use]
pub fn create_random_tree(repo: &Arc<ReadonlyRepo>) -> MergedTreeId {
    let number = rand::random::<u32>();
    let path = RepoPathBuf::from_internal_string(format!("file{number}"));
    create_tree(repo, &[(&path, "contents")]).id()
}

pub fn create_random_commit(mut_repo: &mut MutableRepo) -> CommitBuilder<'_> {
    let tree_id = create_random_tree(mut_repo.base_repo());
    let number = rand::random::<u32>();
    mut_repo
        .new_commit(vec![mut_repo.store().root_commit_id().clone()], tree_id)
        .set_description(format!("random commit {number}"))
}

pub fn commit_with_tree(store: &Arc<Store>, tree_id: MergedTreeId) -> Commit {
    let signature = Signature {
        name: "Some One".to_string(),
        email: "someone@example.com".to_string(),
        timestamp: Timestamp {
            timestamp: MillisSinceEpoch(0),
            tz_offset: 0,
        },
    };
    let commit = backend::Commit {
        parents: vec![store.root_commit_id().clone()],
        predecessors: vec![],
        root_tree: tree_id,
        change_id: ChangeId::from_hex("abcd"),
        description: "description".to_string(),
        author: signature.clone(),
        committer: signature,
        secure_sig: None,
    };
    store.write_commit(commit, None).block_on().unwrap()
}

pub fn dump_tree(store: &Arc<Store>, tree_id: &MergedTreeId) -> String {
    use std::fmt::Write;
    let mut buf = String::new();
    writeln!(
        &mut buf,
        "tree {}",
        tree_id
            .to_merge()
            .iter()
            .map(|tree_id| tree_id.hex())
            .join("&")
    )
    .unwrap();
    let tree = store.get_root_tree(tree_id).unwrap();
    for (path, result) in tree.entries() {
        match result.unwrap().into_resolved() {
            Ok(Some(TreeValue::File { id, executable: _ })) => {
                let file_buf = read_file(store, &path, &id);
                let file_contents = String::from_utf8_lossy(&file_buf);
                writeln!(&mut buf, "  file {path:?} ({id}): {file_contents:?}").unwrap();
            }
            Ok(Some(TreeValue::Symlink(id))) => {
                writeln!(&mut buf, "  symlink {path:?} ({id})").unwrap();
            }
            Ok(Some(TreeValue::GitSubmodule(id))) => {
                writeln!(&mut buf, "  submodule {path:?} ({id})").unwrap();
            }
            entry => {
                unimplemented!("dumping tree entry {entry:?}");
            }
        }
    }
    buf
}

pub fn write_random_commit(mut_repo: &mut MutableRepo) -> Commit {
    create_random_commit(mut_repo).write().unwrap()
}

pub fn write_working_copy_file(workspace_root: &Path, path: &RepoPath, contents: &str) {
    let path = path.to_fs_path(workspace_root).unwrap();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .unwrap();
    file.write_all(contents.as_bytes()).unwrap();
}

pub struct CommitGraphBuilder<'repo> {
    mut_repo: &'repo mut MutableRepo,
}

impl<'repo> CommitGraphBuilder<'repo> {
    pub fn new(mut_repo: &'repo mut MutableRepo) -> Self {
        CommitGraphBuilder { mut_repo }
    }

    pub fn initial_commit(&mut self) -> Commit {
        write_random_commit(self.mut_repo)
    }

    pub fn commit_with_parents(&mut self, parents: &[&Commit]) -> Commit {
        let parent_ids = parents
            .iter()
            .map(|commit| commit.id().clone())
            .collect_vec();
        create_random_commit(self.mut_repo)
            .set_parents(parent_ids)
            .write()
            .unwrap()
    }
}

fn assert_in_rebased_map(
    repo: &impl Repo,
    rebased: &HashMap<CommitId, CommitId>,
    expected_old_commit: &Commit,
) -> Commit {
    let new_commit_id = rebased.get(expected_old_commit.id()).unwrap_or_else(|| {
        panic!(
            "Expected commit to have been rebased: {}",
            expected_old_commit.id().hex()
        )
    });
    let new_commit = repo.store().get_commit(new_commit_id).unwrap().clone();
    new_commit
}

pub fn assert_rebased_onto(
    repo: &impl Repo,
    rebased: &HashMap<CommitId, CommitId>,
    expected_old_commit: &Commit,
    expected_new_parent_ids: &[&CommitId],
) -> Commit {
    let new_commit = assert_in_rebased_map(repo, rebased, expected_old_commit);
    assert_eq!(
        new_commit.parent_ids().to_vec(),
        expected_new_parent_ids
            .iter()
            .map(|x| (*x).clone())
            .collect_vec()
    );
    assert_eq!(new_commit.change_id(), expected_old_commit.change_id());
    new_commit
}

/// Maps children of an abandoned commit to a new rebase target.
///
/// If `expected_old_commit` was abandoned, the `rebased` map indicates the
/// commit the children of `expected_old_commit` should be rebased to, which
/// would have a different change id. This happens when the EmptyBehavior in
/// RebaseOptions is not the default; because of the details of the
/// implementation this returned parent commit is always singular.
pub fn assert_abandoned_with_parent(
    repo: &impl Repo,
    rebased: &HashMap<CommitId, CommitId>,
    expected_old_commit: &Commit,
    expected_new_parent_id: &CommitId,
) -> Commit {
    let new_parent_commit = assert_in_rebased_map(repo, rebased, expected_old_commit);
    assert_eq!(new_parent_commit.id(), expected_new_parent_id);
    assert_ne!(
        new_parent_commit.change_id(),
        expected_old_commit.change_id()
    );
    new_parent_commit
}

pub fn assert_no_forgotten_test_files(test_dir: &Path) {
    let runner_path = test_dir.join("runner.rs");
    let runner = fs::read_to_string(&runner_path).unwrap();
    let entries = fs::read_dir(test_dir).unwrap();
    for entry in entries {
        let path = entry.unwrap().path();
        if let Some(ext) = path.extension() {
            let name = path.file_stem().unwrap();
            if ext == "rs" && name != "runner" {
                let search = format!("mod {};", name.to_str().unwrap());
                assert!(
                    runner.contains(&search),
                    "missing `{search}` declaration in {}",
                    runner_path.display()
                );
            }
        }
    }
}

fn test_init_wasm() {
    println!("Testing init in WASM environment...");

    // Initialize with test settings
    let settings = user_settings();
    let test_workspace = TestWorkspace::init(&settings);
    let repo = &test_workspace.repo;

    // Test the contents of the working-copy commit after init
    let wc_commit_id = repo
        .view()
        .get_wc_commit_id(&WorkspaceId::default())
        .unwrap();
    let wc_commit = repo.store().get_commit(wc_commit_id).unwrap();

    // Verify the initial state
    assert_eq!(*wc_commit.tree_id(), repo.store().empty_merged_tree_id());
    assert_eq!(
        wc_commit.store_commit().parents,
        vec![repo.store().root_commit_id().clone()]
    );
    assert!(wc_commit.predecessors().next().is_none());
    assert_eq!(wc_commit.description(), "");

    // Check user settings were applied
    assert_eq!(wc_commit.author().name, settings.user_name());
    assert_eq!(wc_commit.author().email, settings.user_email());
    assert_eq!(wc_commit.committer().name, settings.user_name());
    assert_eq!(wc_commit.committer().email, settings.user_email());

    println!("✅ Init test passed!");
}

pub fn main() {
    println!("🔬 Running JJ WASM test suite...");

    test_init_wasm();

    // Initialize a test repo
    let test_repo = TestRepo::init();

    // // Create a simple file structure
    let store = test_repo.repo.store();
    let mut tree_builder = store.tree_builder(store.empty_tree_id().clone());

    // Create a test file
    let test_path = RepoPath::from_internal_string("product_metadata.json");
    write_normal_file(
        &mut tree_builder,
        &test_path,
        r#"{"id": "123", "name": "Test Product", "category": "test"}"#,
    );

    // Write the tree and verify
    let tree_id = tree_builder.write_tree().unwrap();
    let tree = store.get_tree(RepoPathBuf::root(), &tree_id).unwrap();

    // Dump the tree contents
    let tree_dump = dump_tree(&store, &MergedTreeId::Legacy(tree_id));
    println!("\n📁 Repository contents:\n{}", tree_dump);

    println!("✅ Test completed successfully!");
}
