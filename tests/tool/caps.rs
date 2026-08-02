use super::*;
use crate::tool::ToolCtx;

/// A registry over the full built-in tool set, for tests that need a real
/// `ToolCtx` but don't care about the registry's contents. Kept here so a
/// future ctx field (F1) touches one construction site, not every test helper.
pub fn test_registry() -> std::sync::Arc<ToolRegistry> {
    std::sync::Arc::new(ToolRegistry::new(crate::tools::tools()))
}

impl Capabilities {
    /// Capabilities over the full built-in registry and a denying Approver, for
    /// tests that need a real [`crate::tool::ToolCtx`] but no live approval
    /// channel. The single test construction site, so a future capability touches
    /// one place rather than every tool test helper.
    pub fn for_test() -> Self {
        Capabilities {
            registry: test_registry(),
            read_cache: Arc::new(FileReadCache::new()),
            approver: Arc::new(DenyingApprover),
            side_query: Arc::new(DenyingSideQuery),
            questioner: Arc::new(DecliningQuestioner),
            subagents: Arc::new(UnavailableSubagentSpawner),
            bg_shells: Arc::new(UnavailableBackgroundShellSpawner),
        }
    }

    /// Capabilities over a caller-supplied registry (and denying effect seams),
    /// for the `tool_search` tests that build a registry with specific deferral
    /// flags. Mirrors [`Capabilities::for_test`] but lets the caller pin the
    /// registry contents.
    pub fn for_test_with_registry(registry: Arc<ToolRegistry>) -> Self {
        Capabilities {
            registry,
            read_cache: Arc::new(FileReadCache::new()),
            approver: Arc::new(DenyingApprover),
            side_query: Arc::new(DenyingSideQuery),
            questioner: Arc::new(DecliningQuestioner),
            subagents: Arc::new(UnavailableSubagentSpawner),
            bg_shells: Arc::new(UnavailableBackgroundShellSpawner),
        }
    }

    /// Capabilities over a caller-supplied [`SideQuery`] (and the full built-in
    /// registry + a denying Approver), for the web_fetch tests that inject a
    /// scripted side-query and assert what it received. The single side-query
    /// construction site, so a future capability touches one place.
    pub fn for_test_with_side_query(side_query: Arc<dyn SideQuery>) -> Self {
        Capabilities {
            registry: test_registry(),
            read_cache: Arc::new(FileReadCache::new()),
            approver: Arc::new(DenyingApprover),
            side_query,
            questioner: Arc::new(DecliningQuestioner),
            subagents: Arc::new(UnavailableSubagentSpawner),
            bg_shells: Arc::new(UnavailableBackgroundShellSpawner),
        }
    }

    /// Capabilities over a caller-supplied [`Questioner`] (and the full built-in
    /// registry + denying Approver/SideQuery), for the `ask_user_question` tests
    /// that inject a scripted questioner and assert what it received/returned. The
    /// single questioner construction site, so a future capability touches one
    /// place.
    pub fn for_test_with_questioner(questioner: Arc<dyn Questioner>) -> Self {
        Capabilities {
            registry: test_registry(),
            read_cache: Arc::new(FileReadCache::new()),
            approver: Arc::new(DenyingApprover),
            side_query: Arc::new(DenyingSideQuery),
            questioner,
            subagents: Arc::new(UnavailableSubagentSpawner),
            bg_shells: Arc::new(UnavailableBackgroundShellSpawner),
        }
    }

    /// Capabilities over a caller-supplied [`SubagentSpawner`] (and the full
    /// built-in registry + denying Approver/SideQuery/Questioner), for the
    /// `agent`-tool tests that inject a scripted spawner and assert what it
    /// received/returned. The single subagents construction site, so a future
    /// consumer touches one place.
    pub fn for_test_with_subagents(subagents: Arc<dyn SubagentSpawner>) -> Self {
        Capabilities {
            registry: test_registry(),
            read_cache: Arc::new(FileReadCache::new()),
            approver: Arc::new(DenyingApprover),
            side_query: Arc::new(DenyingSideQuery),
            questioner: Arc::new(DecliningQuestioner),
            subagents,
            bg_shells: Arc::new(UnavailableBackgroundShellSpawner),
        }
    }

    /// Capabilities over a caller-supplied [`BackgroundShellSpawner`] (and the
    /// full built-in registry + denying effect seams), for the `run_command`
    /// background-branch tests that inject a scripted spawner and assert what it
    /// received/returned. The single bg-shells construction site, so a future
    /// consumer touches one place.
    pub fn for_test_with_bg_shells(bg_shells: Arc<dyn BackgroundShellSpawner>) -> Self {
        Capabilities {
            registry: test_registry(),
            read_cache: Arc::new(FileReadCache::new()),
            approver: Arc::new(DenyingApprover),
            side_query: Arc::new(DenyingSideQuery),
            questioner: Arc::new(DecliningQuestioner),
            subagents: Arc::new(UnavailableSubagentSpawner),
            bg_shells,
        }
    }

    /// Capabilities over a caller-supplied [`FileReadCache`] (and the full
    /// built-in registry + denying effect seams), for the notebook_edit tests
    /// that pre-populate the cache (or share it with read_file) and assert the
    /// read-before-edit enforcement. The single read-cache construction site, so
    /// a future consumer touches one place.
    pub fn for_test_with_read_cache(read_cache: Arc<FileReadCache>) -> Self {
        Capabilities {
            registry: test_registry(),
            read_cache,
            approver: Arc::new(DenyingApprover),
            side_query: Arc::new(DenyingSideQuery),
            questioner: Arc::new(DecliningQuestioner),
            subagents: Arc::new(UnavailableSubagentSpawner),
            bg_shells: Arc::new(UnavailableBackgroundShellSpawner),
        }
    }
}

/// A fake real [`Approver`] that answers with a fixed decision. Stands in for
/// the tx-backed `AgentApprover` in a unit test: proves `Capabilities` holds
/// a real answer behind `Arc<dyn Approver>` without wiring an Agent mpsc.
struct FakeApprover {
    answer: bool,
}

#[async_trait::async_trait]
impl Approver for FakeApprover {
    async fn approve(&self, _id: String, _command: String) -> bool {
        self.answer
    }
}

#[tokio::test]
async fn denying_approver_denies() {
    let approver = DenyingApprover;
    assert!(
        !approver
            .approve("id".to_string(), "rm -rf /".to_string())
            .await
    );
}

/// A fake real [`SideQuery`] that answers with a fixed reply text. Proves
/// `Capabilities` holds a real answer behind `Arc<dyn SideQuery>` without
/// wiring an Llm.
struct FakeSideQuery {
    reply: String,
}

#[async_trait::async_trait]
impl SideQuery for FakeSideQuery {
    async fn run(&self, _request: SideQueryRequest) -> Result<String, String> {
        Ok(self.reply.clone())
    }
}

#[tokio::test]
async fn a_real_approver_returns_its_injected_decision() {
    let caps = Capabilities {
        registry: test_registry(),
        read_cache: Arc::new(FileReadCache::new()),
        approver: Arc::new(FakeApprover { answer: true }),
        side_query: Arc::new(DenyingSideQuery),
        questioner: Arc::new(DecliningQuestioner),
        subagents: Arc::new(UnavailableSubagentSpawner),
        bg_shells: Arc::new(UnavailableBackgroundShellSpawner),
    };
    // The seam is proven with a live wire: the decision travels back through
    // the `Arc<dyn Approver>` the carrier holds.
    assert!(
        caps.approver
            .approve("id".to_string(), "ls".to_string())
            .await
    );
}

#[tokio::test]
async fn denying_side_query_errs() {
    let sq = DenyingSideQuery;
    let err = sq
        .run(SideQueryRequest {
            system: "sys".into(),
            user_content: "content".into(),
            model: None,
            max_attempts: 1,
        })
        .await
        .unwrap_err();
    assert_eq!(err, "side queries are unavailable in this environment");
}

#[tokio::test]
async fn a_real_side_query_returns_its_injected_reply() {
    let caps = Capabilities::for_test_with_side_query(Arc::new(FakeSideQuery {
        reply: "extracted".into(),
    }));
    // The seam is proven with a live wire: the reply travels back through the
    // `Arc<dyn SideQuery>` the carrier holds.
    let out = caps
        .side_query
        .run(SideQueryRequest {
            system: "sys".into(),
            user_content: "content".into(),
            model: None,
            max_attempts: 1,
        })
        .await
        .unwrap();
    assert_eq!(out, "extracted");
}

#[test]
fn capabilities_clones_and_debug_prints() {
    let caps = Capabilities {
        registry: test_registry(),
        read_cache: Arc::new(FileReadCache::new()),
        approver: Arc::new(FakeApprover { answer: false }),
        side_query: Arc::new(DenyingSideQuery),
        questioner: Arc::new(DecliningQuestioner),
        subagents: Arc::new(UnavailableSubagentSpawner),
        bg_shells: Arc::new(UnavailableBackgroundShellSpawner),
    };
    let cloned = caps.clone();
    // The clone shares the same handles (Arc), and both print with the
    // registry expanded and the effect seams opaque.
    let rendered = format!("{cloned:?}");
    assert!(rendered.contains("Capabilities"));
    assert!(rendered.contains("read_cache"));
    assert!(rendered.contains("approver"));
    assert!(rendered.contains("side_query"));
    assert!(rendered.contains("questioner"));
    assert!(rendered.contains("subagents"));
    assert!(rendered.contains("bg_shells"));
    assert!(rendered.contains("<dyn>"));
}

#[tokio::test]
async fn unavailable_background_shell_spawner_errs_on_spawn() {
    let spawner = UnavailableBackgroundShellSpawner;
    let err = spawner
        .spawn_background("echo hi".into(), "/tmp".into())
        .await
        .unwrap_err();
    assert_eq!(err, "background shells are unavailable in this environment");
}

#[tokio::test]
async fn unavailable_background_shell_spawner_stop_is_not_found() {
    let spawner = UnavailableBackgroundShellSpawner;
    let out = spawner.stop_background("bg_1".into()).await.unwrap();
    assert_eq!(out, "Error: No background task found with ID \"bg_1\".");
}

/// A fake real [`SubagentSpawner`] that answers with a fixed result. Proves
/// `Capabilities` holds a real answer behind `Arc<dyn SubagentSpawner>`
/// without wiring a child Run.
struct FakeSpawner {
    result: String,
}

#[async_trait::async_trait]
impl SubagentSpawner for FakeSpawner {
    async fn spawn(&self, _request: SubagentRequest) -> Result<SubagentResult, String> {
        Ok(SubagentResult {
            terminate_reason: "GOAL".into(),
            result: self.result.clone(),
        })
    }

    async fn spawn_background(
        &self,
        _request: SubagentRequest,
        _description: String,
    ) -> Result<String, String> {
        Ok("general-purpose-1".into())
    }

    async fn stop_background(&self, id: String) -> Result<String, String> {
        Ok(format!("Error: No background task found with ID \"{id}\"."))
    }
}

#[tokio::test]
async fn unavailable_subagent_spawner_errs() {
    let spawner = UnavailableSubagentSpawner;
    let err = spawner
        .spawn(SubagentRequest {
            subagent_type: "general-purpose".into(),
            prompt: "do a thing".into(),
            model: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err, "subagents are unavailable in this environment");
}

#[tokio::test]
async fn unavailable_subagent_spawner_background_errs() {
    let spawner = UnavailableSubagentSpawner;
    let err = spawner
        .spawn_background(
            SubagentRequest {
                subagent_type: "general-purpose".into(),
                prompt: "do a thing".into(),
                model: None,
            },
            "do a thing".into(),
        )
        .await
        .unwrap_err();
    assert_eq!(err, "subagents are unavailable in this environment");
}

#[tokio::test]
async fn unavailable_subagent_spawner_stop_is_not_found() {
    let spawner = UnavailableSubagentSpawner;
    let out = spawner.stop_background("scout-1".into()).await.unwrap();
    assert_eq!(out, "Error: No background task found with ID \"scout-1\".");
}

#[tokio::test]
async fn a_real_subagent_spawner_returns_its_injected_result() {
    let caps = Capabilities::for_test_with_subagents(Arc::new(FakeSpawner {
        result: "the findings".into(),
    }));
    // The seam is proven with a live wire: the result travels back through
    // the `Arc<dyn SubagentSpawner>` the carrier holds.
    let out = caps
        .subagents
        .spawn(SubagentRequest {
            subagent_type: "general-purpose".into(),
            prompt: "investigate".into(),
            model: None,
        })
        .await
        .unwrap();
    assert_eq!(out.terminate_reason, "GOAL");
    assert_eq!(out.result, "the findings");
}

/// A fake real [`Questioner`] that answers with fixed picks. Proves
/// `Capabilities` holds a real answer behind `Arc<dyn Questioner>` without
/// wiring an Agent mpsc.
struct FakeQuestioner {
    answers: Vec<(usize, String)>,
}

#[async_trait::async_trait]
impl Questioner for FakeQuestioner {
    async fn ask(&self, _questions: Vec<Question>) -> Result<Vec<(usize, String)>, String> {
        Ok(self.answers.clone())
    }
}

fn a_question() -> Question {
    Question {
        question: "Which library should we use?".into(),
        header: "Library".into(),
        options: vec![
            QuestionOption {
                label: "serde".into(),
                description: "the de-facto standard".into(),
            },
            QuestionOption {
                label: "miniserde".into(),
                description: "smaller, fewer features".into(),
            },
        ],
        multi_select: false,
    }
}

#[tokio::test]
async fn declining_questioner_returns_the_verbatim_non_interactive_string() {
    let q = DecliningQuestioner;
    let err = q.ask(vec![a_question()]).await.unwrap_err();
    assert_eq!(
        err,
        "Cannot ask user questions in non-interactive mode without ACP support. \
             Please run in interactive mode or enable ACP mode to use this tool."
    );
}

#[tokio::test]
async fn a_real_questioner_returns_its_injected_answers() {
    let caps = Capabilities::for_test_with_questioner(Arc::new(FakeQuestioner {
        answers: vec![(0, "serde".into())],
    }));
    // The seam is proven with a live wire: the picks travel back through the
    // `Arc<dyn Questioner>` the carrier holds.
    let out = caps.questioner.ask(vec![a_question()]).await.unwrap();
    assert_eq!(out, vec![(0, "serde".to_string())]);
}

#[test]
fn capabilities_is_send() {
    // Compile-proof: the carrier must cross the `tokio::spawn` at the Agent,
    // so `Arc<dyn Approver>` must be Send + Sync.
    fn assert_send<T: Send>() {}
    assert_send::<Capabilities>();
}

#[test]
fn tool_ctx_for_test_constructs_clones_and_debug_prints() {
    let ctx = ToolCtx::for_test("/nowhere".into(), 10_000);
    let cloned = ctx.clone();
    let rendered = format!("{cloned:?}");
    assert!(rendered.contains("ToolCtx"));
    assert!(rendered.contains("Capabilities"));
}
