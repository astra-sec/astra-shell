use std::collections::BTreeMap;

#[cfg(test)]
use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use prost::Message;

use crate::{
    protocol::{TerminalStateAck, TerminalStateDiff, TerminalStateRow, terminal_state_row},
    terminal_state_v2::{self, Anchor, Row, Screen, State},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AckDisposition {
    Accepted,
    Duplicate,
}

pub(crate) enum PreparedStateUpdate {
    Snapshot(State),
    Diff(TerminalStateDiff),
}

/// Per-attachment reliable synchronization window. At most one generation is
/// in flight. Output arriving while that generation is unacknowledged is
/// represented by one dirty bit, so intermediate states do not form a queue.
#[derive(Default)]
pub(crate) struct StateSyncWindow {
    acknowledged: Option<State>,
    in_flight: Option<State>,
    dirty: bool,
}

impl StateSyncWindow {
    pub(crate) fn begin_initial(&mut self, state: State) -> Result<()> {
        ensure!(
            self.in_flight.is_none(),
            "terminal state is already in flight"
        );
        self.in_flight = Some(state);
        Ok(())
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub(crate) fn needs_update(&self) -> bool {
        self.dirty && self.in_flight.is_none()
    }

    pub(crate) fn prepare_update(
        &mut self,
        latest: State,
        allow_diff: bool,
    ) -> Result<Option<PreparedStateUpdate>> {
        ensure!(
            self.in_flight.is_none(),
            "terminal state ACK is still pending"
        );
        self.dirty = false;
        if let Some(base) = &self.acknowledged {
            if base.epoch == latest.epoch && latest.generation <= base.generation {
                return Ok(None);
            }
        }

        let update = if allow_diff {
            self.acknowledged
                .as_ref()
                .filter(|base| base.epoch == latest.epoch && latest.generation > base.generation)
                .map(|base| terminal_state_diff(base, &latest))
                .transpose()?
                .filter(|diff| diff.encoded_len() < latest.encoded_len())
                .map_or_else(
                    || PreparedStateUpdate::Snapshot(latest.clone()),
                    PreparedStateUpdate::Diff,
                )
        } else {
            PreparedStateUpdate::Snapshot(latest.clone())
        };
        self.in_flight = Some(latest);
        Ok(Some(update))
    }

    pub(crate) fn acknowledge(&mut self, ack: &TerminalStateAck) -> Result<AckDisposition> {
        ensure!(
            ack.epoch.len() == terminal_state_v2::EPOCH_BYTES,
            "terminal state ACK epoch is invalid"
        );
        ensure!(ack.generation > 0, "terminal state ACK generation is zero");

        if self
            .acknowledged
            .as_ref()
            .is_some_and(|state| state.epoch == ack.epoch && ack.generation <= state.generation)
        {
            return Ok(AckDisposition::Duplicate);
        }

        let sent = self
            .in_flight
            .as_ref()
            .context("terminal state ACK has no matching in-flight update")?;
        ensure!(
            sent.epoch == ack.epoch && sent.generation == ack.generation,
            "terminal state ACK does not match the in-flight generation"
        );
        self.acknowledged = self.in_flight.take();
        Ok(AckDisposition::Accepted)
    }
}

pub(crate) fn terminal_state_diff(base: &State, target: &State) -> Result<TerminalStateDiff> {
    terminal_state_v2::validate(base).context("base terminal state is invalid")?;
    terminal_state_v2::validate(target).context("target terminal state is invalid")?;
    ensure!(
        base.epoch == target.epoch,
        "terminal state diff crosses epochs"
    );
    ensure!(
        target.generation > base.generation,
        "terminal state diff target does not advance generation"
    );

    let base_primary = base
        .primary
        .as_ref()
        .context("base primary screen is missing")?;
    let base_alternate = base
        .alternate
        .as_ref()
        .context("base alternate screen is missing")?;
    let target_primary = target
        .primary
        .as_ref()
        .context("target primary screen is missing")?;
    let target_alternate = target
        .alternate
        .as_ref()
        .context("target alternate screen is missing")?;

    let mut metadata = target.clone();
    metadata
        .primary
        .as_mut()
        .expect("validated state has primary screen")
        .included_rows
        .clear();
    metadata
        .alternate
        .as_mut()
        .expect("validated state has alternate screen")
        .included_rows
        .clear();

    Ok(TerminalStateDiff {
        epoch: target.epoch.clone(),
        base_generation: base.generation,
        target_generation: target.generation,
        target_metadata: Some(metadata),
        primary_rows: diff_rows(base_primary, target_primary)?,
        alternate_rows: diff_rows(base_alternate, target_alternate)?,
    })
}

#[cfg(test)]
pub(crate) fn apply_terminal_state_diff(base: &State, diff: &TerminalStateDiff) -> Result<State> {
    terminal_state_v2::validate(base).context("base terminal state is invalid")?;
    ensure!(
        diff.epoch.len() == terminal_state_v2::EPOCH_BYTES,
        "terminal state diff epoch is invalid"
    );
    ensure!(
        base.epoch == diff.epoch,
        "terminal state diff base epoch changed"
    );
    ensure!(
        base.generation == diff.base_generation,
        "terminal state diff base generation does not match"
    );
    ensure!(
        diff.target_generation > diff.base_generation,
        "terminal state diff target does not advance generation"
    );
    let mut target = diff
        .target_metadata
        .clone()
        .context("terminal state diff target metadata is missing")?;
    ensure!(
        target.epoch == diff.epoch,
        "terminal state diff target epoch changed"
    );
    ensure!(
        target.generation == diff.target_generation,
        "terminal state diff target generation does not match metadata"
    );
    ensure!(
        target
            .primary
            .as_ref()
            .is_some_and(|screen| screen.included_rows.is_empty())
            && target
                .alternate
                .as_ref()
                .is_some_and(|screen| screen.included_rows.is_empty()),
        "terminal state diff metadata contains full rows"
    );

    target
        .primary
        .as_mut()
        .expect("checked primary metadata")
        .included_rows = apply_rows(
        base.primary
            .as_ref()
            .context("base primary screen is missing")?,
        &diff.primary_rows,
    )?;
    target
        .alternate
        .as_mut()
        .expect("checked alternate metadata")
        .included_rows = apply_rows(
        base.alternate
            .as_ref()
            .context("base alternate screen is missing")?,
        &diff.alternate_rows,
    )?;
    terminal_state_v2::validate(&target).context("reconstructed terminal state is invalid")?;
    Ok(target)
}

fn diff_rows(base: &Screen, target: &Screen) -> Result<Vec<TerminalStateRow>> {
    let base_rows = rows_by_anchor(&base.included_rows)?;
    target
        .included_rows
        .iter()
        .map(|row| {
            let anchor = row.start.as_ref().context("target row has no anchor")?;
            let source = if base_rows
                .get(&anchor_key(anchor))
                .is_some_and(|base_row| *base_row == row)
            {
                terminal_state_row::Source::BaseAnchor(anchor.clone())
            } else {
                terminal_state_row::Source::Replacement(row.clone())
            };
            Ok(TerminalStateRow {
                source: Some(source),
            })
        })
        .collect()
}

#[cfg(test)]
fn apply_rows(base: &Screen, rows: &[TerminalStateRow]) -> Result<Vec<Row>> {
    ensure!(
        rows.len() <= terminal_state_v2::MAX_INCLUDED_ROWS,
        "terminal state diff contains too many rows"
    );
    let base_rows = rows_by_anchor(&base.included_rows)?;
    let mut seen = BTreeSet::new();
    rows.iter()
        .map(|row| {
            let reconstructed = match row
                .source
                .as_ref()
                .context("terminal state diff row source is missing")?
            {
                terminal_state_row::Source::BaseAnchor(anchor) => base_rows
                    .get(&anchor_key(anchor))
                    .cloned()
                    .cloned()
                    .context("terminal state diff references an unavailable base row")?,
                terminal_state_row::Source::Replacement(row) => row.clone(),
            };
            let anchor = reconstructed
                .start
                .as_ref()
                .context("terminal state diff row has no anchor")?;
            ensure!(
                seen.insert(anchor_key(anchor)),
                "terminal state diff repeats a row anchor"
            );
            Ok(reconstructed)
        })
        .collect()
}

fn rows_by_anchor(rows: &[Row]) -> Result<BTreeMap<(u64, u32), &Row>> {
    let mut result = BTreeMap::new();
    for row in rows {
        let anchor = row
            .start
            .as_ref()
            .context("terminal state row has no anchor")?;
        ensure!(
            result.insert(anchor_key(anchor), row).is_none(),
            "terminal state repeats a row anchor"
        );
    }
    Ok(result)
}

fn anchor_key(anchor: &Anchor) -> (u64, u32) {
    (anchor.logical_line_id, anchor.cell_offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_engine::TerminalEngine;

    #[test]
    fn cumulative_diff_reconstructs_latest_state_and_is_smaller_for_one_changed_row() {
        let mut engine = TerminalEngine::new(24, 80, 512, Box::new(std::io::sink())).unwrap();
        for index in 0..200 {
            engine.advance(format!("line {index:03}\r\n").as_bytes());
        }
        let base = engine.semantic_state().unwrap();
        engine.advance(b"changed");
        let target = engine.semantic_state().unwrap();

        let diff = terminal_state_diff(&base, &target).unwrap();
        let reconstructed = apply_terminal_state_diff(&base, &diff).unwrap();
        assert_eq!(reconstructed, target);
        println!(
            "semantic-state-bytes snapshot={} cumulative-diff={}",
            target.encoded_len(),
            diff.encoded_len()
        );
        assert!(diff.encoded_len() * 4 < target.encoded_len());
    }

    #[test]
    fn synchronization_window_keeps_one_update_in_flight_and_discards_intermediate_states() {
        let mut engine = TerminalEngine::new(24, 80, 512, Box::new(std::io::sink())).unwrap();
        for index in 0..200 {
            engine.advance(format!("line {index:03}\r\n").as_bytes());
        }
        let initial = engine.semantic_state().unwrap();
        let mut window = StateSyncWindow::default();
        window.begin_initial(initial.clone()).unwrap();
        for _ in 0..100 {
            window.mark_dirty();
        }
        assert!(!window.needs_update());

        let initial_ack = TerminalStateAck {
            epoch: initial.epoch.clone(),
            generation: initial.generation,
        };
        assert_eq!(
            window.acknowledge(&initial_ack).unwrap(),
            AckDisposition::Accepted
        );
        assert_eq!(
            window.acknowledge(&initial_ack).unwrap(),
            AckDisposition::Duplicate
        );
        assert!(window.needs_update());

        engine.advance(b"one\r\ntwo\r\nthree");
        let latest = engine.semantic_state().unwrap();
        let update = window
            .prepare_update(latest.clone(), true)
            .unwrap()
            .unwrap();
        let PreparedStateUpdate::Diff(diff) = update else {
            panic!("expected a cumulative diff")
        };
        assert_eq!(diff.target_generation, latest.generation);
        assert!(!window.needs_update());
    }

    #[test]
    fn diff_rejects_the_wrong_base_without_mutating_it() {
        let mut engine = TerminalEngine::new(2, 8, 8, Box::new(std::io::sink())).unwrap();
        let base = engine.semantic_state().unwrap();
        engine.advance(b"x");
        let target = engine.semantic_state().unwrap();
        let diff = terminal_state_diff(&base, &target).unwrap();
        let mut wrong = base.clone();
        wrong.generation += 1;
        assert!(apply_terminal_state_diff(&wrong, &diff).is_err());
        assert_eq!(base.generation, diff.base_generation);
    }

    #[test]
    fn cumulative_diff_preserves_scroll_append_and_trim_anchor_order() {
        let mut engine = TerminalEngine::new(3, 20, 5, Box::new(std::io::sink())).unwrap();
        engine.advance(b"zero\r\none\r\ntwo\r\nthree\r\nfour");
        let base = engine.semantic_state().unwrap();
        engine.advance(b"\r\nfive\r\nsix\r\nseven\r\neight");
        let target = engine.semantic_state().unwrap();
        assert_eq!(base.epoch, target.epoch);
        assert_ne!(
            base.primary.as_ref().unwrap().included_start,
            target.primary.as_ref().unwrap().included_start
        );

        let diff = terminal_state_diff(&base, &target).unwrap();
        assert_eq!(apply_terminal_state_diff(&base, &diff).unwrap(), target);
    }
}
