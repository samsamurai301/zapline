# zapline-codegen

Typed API clients from [zapline](https://github.com/samsamurai301/zapline)'s
OpenAPI 3.1 documents:

- `typescript(&spec)`: one dependency-free `.ts` file (interfaces plus a typed
  `createClient()`), which `zapline dev` keeps current in `frontend/src/api.ts`;
- `javascript(&spec)`: the same client as an ES module with JSDoc types, which
  `OpenApi::client_js()` serves at `/_zapline/api.js` for pages without a build
  step.

Both use `fetch`, throw `ApiError` for error responses and a `ValidationError`
whose `fields` are typed by the request body for 422s, and pass
`tsc --strict --exactOptionalPropertyTypes --noUncheckedIndexedAccess`.

You normally use it through `zapline::openapi::codegen` or the `zapline client`
command. See the guide's [Typed frontends](https://samsamurai301.github.io/zapline/frontend.html)
page.
