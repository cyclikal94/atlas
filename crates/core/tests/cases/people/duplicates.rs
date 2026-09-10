use anyhow::Result;
use atlas_core::policy::Policy;

use crate::support::task_workflows::{account, id, setup};

#[tokio::test]
async fn person_duplicate_suggestions_only_scan_visible_names() -> Result<()> {
    let (s, _dir) = setup().await?;
    let a = account(&s).await?;
    let b = account(&s).await?;
    let first = id();
    let duplicate = id();
    let hidden = id();
    for (owner, id, name) in [
        (&a, &first, "Morgan Jones"),
        (&a, &duplicate, "morgan   JONES"),
        (&b, &hidden, "Morgan Jones"),
    ] {
        s.apply(
            owner,
            &self::id(),
            &[atlas_core::Command::CreatePerson {
                id: id.clone(),
                name: name.into(),
                initial_policy: Some(Policy::default()),
            }],
        )
        .await?;
    }
    let mut found = Vec::new();
    let mut after = None;
    loop {
        let page = s.person_duplicates(&a, &first, after.as_deref(), 1).await?;
        found.extend(page.items.into_iter().map(|p| p.id));
        after = page.next_after;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(found, vec![duplicate]);
    assert!(s.person_duplicates(&a, &hidden, None, 10).await.is_err());
    Ok(())
}
