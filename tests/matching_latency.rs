//! Isolated warm matching regression. Run with --release --ignored --nocapture.
use qnav::{app::App, model::*};
use std::time::{Duration, Instant};

#[test]
#[ignore = "release performance qualification; run explicitly"]
fn warm_matching_has_a_bounded_cpu_budget() {
    let roots: Vec<_> = (1..=20)
        .map(|id| RootEntry {
            id,
            path: format!("/work/root{id:02}").into(),
        })
        .collect();
    let mut app = App::new(roots, ExplorationPolicy { home: None });
    app.change_depth(true);
    for id in 1..=20 {
        let root = NodeKey::root(id);
        for branch in 0..25 {
            let key = root.child(format!("branch{branch:02}").as_ref());
            app.nodes
                .get_mut(&root)
                .unwrap()
                .children
                .insert(key.relative.file_name().unwrap().into(), key.clone());
            let mut node = DirectoryNode::new(key.clone());
            node.listing = ListingState::Ready;
            app.nodes.insert(key.clone(), node);
            for leaf in 0..19 {
                let name = if id == 1 && branch == 0 && leaf == 0 {
                    "needle".into()
                } else {
                    format!("leaf{leaf:02}")
                };
                let child = key.child(name.as_ref());
                app.nodes
                    .get_mut(&key)
                    .unwrap()
                    .children
                    .insert(name.into(), child.clone());
                app.nodes.insert(child.clone(), DirectoryNode::new(child));
            }
        }
        app.nodes.get_mut(&root).unwrap().listing = ListingState::Ready;
    }
    let mut samples = Vec::new();
    let mut rounds = Vec::new();
    for index in 0..20 {
        let start = Instant::now();
        app.edit_query(if index % 2 == 0 { "needle" } else { "needl" }.into());
        let mut ticks = 1;
        while app.target().is_none() {
            app.match_tick();
            ticks += 1;
            assert!(start.elapsed() < Duration::from_secs(2));
        }
        samples.push(start.elapsed());
        rounds.push(ticks);
    }
    samples.sort();
    rounds.sort();
    println!(
        "10k warm matching CPU P50={:?} P95={:?} max={:?}, P95 ticks={}",
        samples[9], samples[18], samples[19], rounds[18]
    );
    assert!(
        samples[18] <= Duration::from_millis(25),
        "CPU budget leaves room for input/render within 50ms"
    );
}
