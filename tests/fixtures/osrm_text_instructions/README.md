# osrm-text-instructions fixtures (English)

`en_v5_cases.json` is every `test/fixtures/v5/*/*.json` fixture of
[osrm-text-instructions](https://github.com/Project-OSRM/osrm-text-instructions.js)
0.13.4 that has a `step` and an English sentence, reduced to `step`,
`options` and `en`. Regenerate it with the script in
`docs/superpowers/plans/2026-09-25-turn-by-turn-navigation.md`, Task 3, Step 1.

BSD-2-Clause, Copyright (c) 2016, Project OSRM contributors -- see
`LICENSE.txt` beside this file. `src/nav/instructions.rs` is a port of that
project's `index.js`; the phrase table it embeds is
`src/nav/osrm_text_instructions/en.json`, verbatim.
