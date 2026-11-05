# __PROJECT_NAME__

A CS2 TypeScript project (cs_script, `point_script` entities) for the addon `__ADDON__`, scaffolded
by the `cs_script` tool of hammer-mcp from the Source2ZE `create-cs-script` __UPSTREAM__ boilerplate
(MIT, https://github.com/Source2ZE/cs_script; the licence text is in `LICENSE.create-cs-script`).

## Layout

- `src/<name>/index.ts` + `tsconfig.json`: one folder per script. Each builds to
  `../maps/scripts/<name>.js` (the addon's content folder), one `rollup.config.js` entry per script.
- A `point_script` entity points at it with the keyvalue `cs_script` = `maps/scripts/<name>.vjs`.
- The tools session hot-reloads a script when its `.js` changes while the map runs.
- `.js` files are generated: edit the `.ts`, never the `.js`.

## With the MCP tool

`cs_script {action:"build"}` type-checks and builds, `{action:"compile"}` makes the `.vjs_c`,
`{action:"add", name}` adds a script, `{action:"update"}` bumps the `@s2ze/*` packages and re-checks
them against the game's own `point_script.d.ts` (a mismatch writes `types/point_script.d.ts` and
makes the scripts type-check against the game's file until the npm package catches up).

## By hand

```sh
npm install        # install dependencies
npm start          # build and watch for changes
```

Updating the s2ze packages: `npm install @s2ze/debug@latest @s2ze/math@latest @s2ze/prototype-extend@latest @s2ze/scheduler@latest @s2ze/tsconfig@latest @s2ze/types@latest`
