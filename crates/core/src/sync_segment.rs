//! Bounded pieces of one delta revision (BE-Q23). A revision's notices are ordered once, from
//! the immutable payload and immutable resource kinds: an upsert pass in `RESOURCE_ORDER` rank
//! then id, and a removal pass in reverse rank then id, as for a whole batch. A piece is the next
//! consecutive slice of that sequence, resolved against the reader's access at its own request.
use super::*;
use crate::sync_cursor::Pass;

/// Where a piece starts or the next one resumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Position {
    pub pass: Pass,
    pub ordinal: usize,
}
impl Position {
    pub const START: Self = Self {
        pass: Pass::Upsert,
        ordinal: 0,
    };
}

pub(super) struct Piece {
    pub changes: Vec<Change>,
    /// `None` once both passes are exhausted: this piece is the revision's last.
    pub next: Option<Position>,
}

fn rank(kind: &str) -> Option<usize> {
    RESOURCE_ORDER.iter().position(|k| *k == kind)
}

fn placeholders(from: usize, count: usize) -> String {
    (from..from + count)
        .map(|i| format!("${i}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Rows of `statement` (whose ids are an `IN ({})` list after `fixed` leading binds), in chunks
/// of 100 ids, as `(column 0, column 1)` pairs.
async fn pairs(
    tx: &mut Transaction<'_, Any>,
    statement: &str,
    fixed: &[&str],
    ids: &[&String],
) -> Result<Vec<(String, String)>> {
    let mut rows = Vec::new();
    for chunk in ids.chunks(100) {
        let sql = statement.replace("{}", &placeholders(fixed.len() + 1, chunk.len()));
        // Every statement is repository-owned; the ids are always bound.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        for value in fixed {
            query = query.bind(*value);
        }
        for id in chunk {
            query = query.bind(id.as_str());
        }
        for row in query.fetch_all(&mut **tx).await? {
            rows.push((row.get(0), row.get(1)));
        }
    }
    Ok(rows)
}

/// The two pass orders of a revision. A kind outside `RESOURCE_ORDER` is never upserted and is
/// removed with rank 0, exactly as the whole batch treats it.
async fn order(
    tx: &mut Transaction<'_, Any>,
    notices: &[Notice],
) -> Result<(Vec<String>, Vec<String>)> {
    let ids: BTreeSet<&String> = notices.iter().map(|n| &n.id).collect();
    let ids: Vec<&String> = ids.into_iter().collect();
    let kinds: BTreeMap<String, String> = pairs(
        tx,
        "SELECT id,kind FROM resources WHERE id IN ({})",
        &[],
        &ids,
    )
    .await?
    .into_iter()
    .collect();
    let mut seen = BTreeSet::new();
    let mut ranked = Vec::new();
    for notice in notices {
        if seen.insert(&notice.id) {
            let kind = kinds.get(&notice.id).unwrap_or(&notice.resource_kind);
            ranked.push((rank(kind), notice.id.clone()));
        }
    }
    let mut upserts: Vec<_> = ranked
        .iter()
        .filter_map(|(rank, id)| rank.map(|r| (r, id.clone())))
        .collect();
    upserts.sort();
    let mut removals: Vec<_> = ranked
        .into_iter()
        .map(|(rank, id)| (std::cmp::Reverse(rank.unwrap_or(0)), id))
        .collect();
    removals.sort();
    Ok((
        upserts.into_iter().map(|(_, id)| id).collect(),
        removals.into_iter().map(|(_, id)| id).collect(),
    ))
}

/// Build the piece of `notices` that starts at `start` and holds at most `m` changes, `m >= 4`.
/// Each upsert brings every visible ancestor not already in this piece, before it; ancestors
/// count. A removal is a notice that is not visible now and was delivered to this device.
pub(super) async fn build(
    tx: &mut Transaction<'_, Any>,
    actor: &str,
    device: &str,
    notices: &[Notice],
    start: Position,
    m: usize,
) -> Result<Piece> {
    let (upsert_order, removal_order) = order(tx, notices).await?;
    let mut upserts = BTreeMap::<String, Projection>::new();
    let mut removals = Vec::new();
    let mut position = start;
    let next = 'fill: {
        if position.pass == Pass::Upsert {
            while position.ordinal < upsert_order.len() {
                let window: Vec<&String> =
                    upsert_order[position.ordinal..].iter().take(m).collect();
                let visible = Store::subset_batch(
                    tx,
                    actor,
                    &window.iter().map(|id| (*id).clone()).collect(),
                )
                .await?;
                let candidates: Vec<&String> = window
                    .iter()
                    .copied()
                    .filter(|id| visible.contains_key(*id))
                    .collect();
                let mut ancestors = BTreeMap::<String, Vec<String>>::new();
                for (resource, ancestor) in pairs(
                    tx,
                    "SELECT resource_id,ancestor_id FROM resource_ancestors WHERE resource_id IN ({})",
                    &[],
                    &candidates,
                )
                .await?
                {
                    ancestors.entry(resource).or_default().push(ancestor);
                }
                let parents: BTreeSet<String> = ancestors.values().flatten().cloned().collect();
                let parents = Store::subset_batch(tx, actor, &parents).await?;
                for id in window {
                    let Some(resource) = visible.get(id) else {
                        position.ordinal += 1;
                        continue;
                    };
                    let mut needed = BTreeMap::new();
                    for (key, p) in std::iter::once((id, resource)).chain(
                        ancestors
                            .get(id)
                            .into_iter()
                            .flatten()
                            .filter_map(|a| parents.get_key_value(a)),
                    ) {
                        if rank(&p.kind).is_some() && !upserts.contains_key(key) {
                            needed.insert(key.clone(), p.clone());
                        }
                    }
                    if upserts.len() + needed.len() > m {
                        break 'fill Some(position);
                    }
                    upserts.extend(needed);
                    position.ordinal += 1;
                }
            }
            position = Position {
                pass: Pass::Remove,
                ordinal: 0,
            };
        }
        while position.ordinal < removal_order.len() {
            let window: Vec<&String> = removal_order[position.ordinal..].iter().take(m).collect();
            let visible =
                Store::subset_batch(tx, actor, &window.iter().map(|id| (*id).clone()).collect())
                    .await?;
            let delivered: BTreeSet<String> = pairs(
                tx,
                "SELECT resource_id,device_id FROM sync_deliveries WHERE account_id=$1 AND device_id=$2 AND resource_id IN ({})",
                &[actor, device],
                &window,
            )
            .await?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
            for id in window {
                if !visible.contains_key(id) && delivered.contains(id) {
                    if upserts.len() + removals.len() + 1 > m {
                        break 'fill Some(position);
                    }
                    removals.push(Change::Remove { id: id.clone() });
                }
                position.ordinal += 1;
            }
        }
        None
    };
    let mut changes = ordered(upserts);
    changes.extend(removals);
    Ok(Piece { changes, next })
}

/// Parents precede children: `RESOURCE_ORDER` rank, then id.
fn ordered(upserts: BTreeMap<String, Projection>) -> Vec<Change> {
    let mut changes = Vec::with_capacity(upserts.len());
    for kind in RESOURCE_ORDER {
        changes.extend(
            upserts
                .values()
                .filter(|p| p.kind == kind)
                .cloned()
                .map(|resource| Change::Upsert { resource }),
        );
    }
    changes
}
