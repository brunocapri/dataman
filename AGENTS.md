# AGENTS.md

Rules for agents working on Dataman. They are not suggestions: when a change would break one,
stop and ask instead of working around it. Existing code breaks some of them: do not copy it,
and do not clean it up unless that is the task.

Dataman follows data-driven design, always. See [Architecture slop](#architecture-slop).

## UI slop

The UI lives in `crates/dataman`. Colors are the tokens in `theme.rs` (`Theme`); shared
components are in `ui.rs`.

- **Never use gradients.** Not on backgrounds, buttons, borders, text, glows or shadows. Gradients
  are the clearest sign of slop. Every surface is one flat theme color.
- **Badges are a last resort.** Add one only when it is strictly necessary and no other element
  would show the state better: the text itself, its weight or color, an icon, the item's
  position, or the state of a control that is already there. When in doubt, no badge.
- **One accent color.** `t.acc` (and its `acc_*` variants) is the only accent. Nothing competes
  with it, not even something critical like an error: status colors (`err`, `warn`, `ok`,
  `info`) mark the text or icon they are about, never a large filled area, and never a second
  highlight next to the accent.
- **No explanatory text.** If the UI needs a sentence of plain text to explain itself, the UI is
  probably wrong. Fix the design instead: the layout, the naming, the defaults, the control
  itself. Exceptions are rare; when one is truly needed, keep it to one short line.
- **Same action, same look.** An action that does the same thing looks and behaves the same
  everywhere it appears. Copying is always `ui::copy_button`, with the same icon, size and
  "Copied" flash; never a hand-made variant for one screen.
- **Reuse before you build.** Before writing a control, look in `ui.rs`: buttons, icon buttons,
  copy, checkboxes, switches, segmented controls, menus, popovers, tooltips, modals. If nothing
  fits, add the component to `ui.rs` and use it from there, not inline in one view. Colors come
  from `Theme`, never from literals in a view.
- **No tinted pills or callouts.** Never style an element as one loud color plus a background of
  that same color at lower opacity. That means no button or badge with its border and text in
  one color over a faded fill of that color, and no container with a thick left border in a loud
  color over a faded fill of that color.
- **Avoid dividers.** Build hierarchy with spacing, alignment, type size and weight, and text
  color (`tx` to `tx4`). Reach for a line only when none of those work.
- **No underline under tabs.** Never mark an active or focused tab with an underline or accent
  line.

## Code slop

- **Silent fallbacks.** Never let a failure become normal-looking data through
  `unwrap_or_default()`, `.ok()` or `let _ =`. In a database client, an empty list that was really
  a lost connection, or a NULL that was really a bad decode, is a wrong answer shown as the truth.
  Handle the failure in the layer that owns it and make it visible: `ChunkBuilder::push_undecoded`
  stores a NULL, but counts it and logs it. Keep a default only when it is the correct value. Do
  not guard against states that cannot happen either: change the data so they cannot exist.
- **Near-duplicates.** Before writing a helper, type or parser, search for one. The most common
  slop is a second version of something the agent never saw. If the existing one almost fits,
  change it. The same concern is handled the same way in every file, so that knowing one part of
  the code helps read the next.
- **Allocating per item.** In anything that runs per row, per tree node, per keystroke or per
  frame, do not `clone()`, `to_string()`, `format!` or `collect()` for each item. Borrow or reuse:
  `Cell<'_>` borrows from the chunk, and `format_cell_into` refills one `String` the caller owns.
  Use `with_capacity` when the count is known. A `clone()` that only quiets the borrow checker
  means the ownership is wrong, on any path. Elsewhere, plain code wins: do not contort it to save
  an allocation nobody will measure.
- **Ceremony.** Comments that restate the code or narrate the change ("now uses X") and tests that
  cannot fail add volume and only look like care. A comment says why, when the code cannot, like
  the notes in `Theme` on how GPUI adds alpha. A test must fail if the behaviour breaks: assert
  what the user or the database would see, not a mock or a value the test just set. What depends
  on an engine is tested on that engine (`crates/dataman-core/tests/live_*.rs`).

## Architecture slop

`dataman-core` holds the data and its transformations and knows nothing about GPUI. `dataman` draws
it, and `dataman-mcp` serves it to agents.

- **Code before data.** Every program transforms data from one form to another, so start from the
  data: what it is, how much of it there is, which fields are read together, how often and on
  which path. Then write the layout that path needs, not the shape of the domain's nouns. Where
  there is one, there are many: design for the batch, not for one item in a loop. This is why
  results are columnar (`ColData`): one text buffer with `ends` offsets instead of a `String` per
  cell, and `nulls` as a bitmap. The answer is not always columns, and a settings struct read once
  stays a plain struct. The reasoning is what is not optional.
- **Differences written as branches.** What varies between cases belongs in a table the code reads,
  not in `match`es spread across files. `SPECS` in `crates/dataman-core/src/engine.rs` has one row
  per engine: ports, dialect, `Caps`, system schemas, nouns, the connection form. A new fact about
  engines is a new field there, and a view asks `engine.caps()`, never which engine it is on. Keep
  a `match` only where the code itself differs. A table means plain static data, not a DSL or a
  plugin system.
- **Abstractions that rot before they are useful.** An abstraction written before its second use
  rots before its first: it is shaped by a guess, and every real case then has to bend around it.
  No trait with one implementation, factory for one type, layer that only forwards calls, or option
  nobody asked for. Write the concrete code twice, then look at the data both cases share. Splitting
  files is not modularity either: a module owns a piece of data and the functions that transform
  it, and files that must change together are one module.
- **Cost that follows the data, not the screen.** A result can be bigger than memory, and the UI
  thread has a frame to draw. Every path that touches data has a stated bound: rows arrive in
  chunks (`CHUNK`, `prefetch()`), and the grid paints and requests only what is visible. Never do
  I/O or walk a whole result on the UI thread. Anything that grows with use has a cap or streams.
  Performance claims come with numbers, before and after, from the `--bench-*` modes (`checks`
  feature), never as "should be faster".

## Building and testing

- **The edit loop is the dev profile.** `cargo run -p dataman`, `cargo test` and `scripts/dev.sh`
  share `target/debug`. Build `--release` only for the end-to-end stages or a package.
- **Test what you touched.** `cargo test -p <crate> <name>` or `--test live_mysql`. Without the
  servers up, `live_*` tests skip themselves and pass: `docker compose -f e2e/compose.yaml up -d
  --wait <service>` first when the change depends on an engine.
- **A check never retries and never sleeps its way to green.** A stage that fails on CI is a bug,
  in the check or in the app. Wait on the condition with `crate::check::until`, not a fixed pause.
