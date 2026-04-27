use super::*;

#[test]
fn test_get_runnable_workflows() {
    let db = test_db();
    let steps = sample_steps(2);

    db.create_workflow("wf-1", "WF 1", "G1", &steps, None, None, None, None)
        .unwrap();
    db.create_workflow("wf-2", "WF 2", "G2", &steps, None, None, None, None)
        .unwrap();

    let runnable = db.get_runnable_workflows().unwrap();
    assert_eq!(runnable.len(), 2);
}

#[test]
fn test_get_runnable_excludes_cancelled() {
    let db = test_db();
    let steps = sample_steps(2);

    db.create_workflow("wf-1", "WF 1", "G1", &steps, None, None, None, None)
        .unwrap();
    db.create_workflow("wf-2", "WF 2", "G2", &steps, None, None, None, None)
        .unwrap();

    db.cancel_workflow("wf-1").unwrap();

    let runnable = db.get_runnable_workflows().unwrap();
    assert_eq!(runnable.len(), 1);
    assert_eq!(runnable[0].id, "wf-2");
}

#[test]
fn test_get_runnable_excludes_workflows_with_running_step() {
    let db = test_db();
    let steps = sample_steps(2);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Claim a step — now wf has a running step, shouldn't be in runnable
    let _s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();

    let runnable = db.get_runnable_workflows().unwrap();
    assert!(runnable.is_empty());
}

#[test]
fn test_get_runnable_excludes_failed_workflows() {
    let db = test_db();
    let mut steps = sample_steps(2);
    steps[0].max_retries = 1;

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Fail step to exhaust retries → workflow becomes FAILED
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.fail_workflow_step(s.id, "fatal").unwrap();

    let runnable = db.get_runnable_workflows().unwrap();
    assert!(runnable.is_empty());
}

#[test]
fn test_get_runnable_excludes_completed_workflows() {
    let db = test_db();
    let steps = sample_steps(1);

    db.create_workflow("wf-1", "WF", "G", &steps, None, None, None, None)
        .unwrap();

    // Complete the only step → workflow becomes COMPLETED
    let s = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    db.complete_workflow_step(s.id, "done").unwrap();

    let runnable = db.get_runnable_workflows().unwrap();
    assert!(runnable.is_empty());
}

#[test]
fn test_claim_steps_from_multiple_workflows_same_tick() {
    let db = test_db();
    let steps = sample_steps(2);

    db.create_workflow("wf-1", "WF A", "G", &steps, None, None, None, None)
        .unwrap();
    db.create_workflow("wf-2", "WF B", "G", &steps, None, None, None, None)
        .unwrap();

    // Both should be runnable initially
    let runnable = db.get_runnable_workflows().unwrap();
    assert_eq!(runnable.len(), 2);

    // Claim from both (simulating same daemon tick)
    let s1 = db.claim_next_workflow_step("wf-1").unwrap().unwrap();
    let s2 = db.claim_next_workflow_step("wf-2").unwrap().unwrap();
    assert_eq!(s1.workflow_id, "wf-1");
    assert_eq!(s2.workflow_id, "wf-2");

    // Neither should be runnable now (both have running steps)
    let runnable = db.get_runnable_workflows().unwrap();
    assert!(runnable.is_empty());

    // Complete both and verify they advance independently
    db.complete_workflow_step(s1.id, "a1").unwrap();
    db.complete_workflow_step(s2.id, "b1").unwrap();

    let runnable = db.get_runnable_workflows().unwrap();
    assert_eq!(runnable.len(), 2);
}
