//! Scope bindings belong to polling tasks, not newly spawned children.

use kernal_api::async_engine::{launch, TaskLocalAccessError};

kernal_api::task_local! {
    static REQUEST_ID: u64 = REQUEST_ID_STORAGE;
}

#[tokio::test(flavor = "current_thread")]
async fn spawned_work_does_not_inherit_the_callers_task_local_binding() {
    REQUEST_ID
        .scope(42, async {
            assert_eq!(REQUEST_ID.with(|value| *value), 42);
            let child = launch(async { REQUEST_ID.try_with(|value| *value) });
            assert_eq!(
                child.await.expect("child task"),
                Err(TaskLocalAccessError::Unbound)
            );
            assert_eq!(REQUEST_ID.with(|value| *value), 42);
        })
        .await;
    assert_eq!(
        REQUEST_ID.try_with(|value| *value),
        Err(TaskLocalAccessError::Unbound)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn child_can_install_its_own_binding_without_changing_the_parent() {
    REQUEST_ID
        .scope(42, async {
            let child = launch(REQUEST_ID.scope(7, async {
                kernal_api::async_engine::yield_now().await;
                REQUEST_ID.with(|value| *value)
            }));
            assert_eq!(child.await.expect("scoped child"), 7);
            assert_eq!(REQUEST_ID.with(|value| *value), 42);
        })
        .await;
}

mod declaration_shapes {
    //! Declaration and access shapes first-party clients rely on.

    use std::cell::{Cell, RefCell};
    use std::sync::Arc;

    pub(crate) struct ProgressSlot(u32);

    kernal_api::task_local! {
        /// Documented, crate-visible declaration.
        pub(crate) static PROGRESS: Arc<ProgressSlot> = PROGRESS_TLS;
    }
    kernal_api::task_local! {
        pub static LABEL: String = LABEL_TLS;
    }
    kernal_api::task_local! {
        static PHASE: Cell<Option<&'static str>> = PHASE_TLS;
    }
    kernal_api::task_local! {
        static NOTE: RefCell<Option<String>> = NOTE_TLS;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nested_scopes_of_distinct_types_compose() {
        PROGRESS
            .scope(Arc::new(ProgressSlot(3)), async {
                LABEL
                    .scope("unit".to_owned(), async {
                        PHASE
                            .scope(Cell::new(None), async {
                                NOTE.scope(RefCell::new(None), async {
                                    PHASE.with(|phase| phase.set(Some("compile")));
                                    NOTE.with(|note| *note.borrow_mut() = Some("hit".into()));
                                    kernal_api::async_engine::yield_now().await;
                                    assert_eq!(PROGRESS.with(|slot| slot.0), 3);
                                    assert_eq!(LABEL.try_with(Clone::clone).as_deref(), Ok("unit"));
                                    assert_eq!(PHASE.with(Cell::get), Some("compile"));
                                    assert_eq!(
                                        NOTE.with(|note| note.borrow().clone()),
                                        Some("hit".to_owned())
                                    );
                                })
                                .await;
                            })
                            .await;
                    })
                    .await;
            })
            .await;
        assert!(PROGRESS.try_with(|slot| slot.0).is_err());
    }
}
