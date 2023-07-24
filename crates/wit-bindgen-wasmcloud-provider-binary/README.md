# `wit-bindgen-wasmcloud-provider-binary`

This crate contains a macro that helps build [Wasmcloud Capability Providers][wasmcloud-capability-providers] which are distributed as binaries and work with the [WIT][wit].

This crate leverages [`wit-bindgen`][wit-bindgen] in it's generation of interfaces and code.

## Usage

This crate can be be used similarly to `wit-bindgen`, with the following syntax:

```rust
wit_bindgen_wasmcloud::provider::binary::generate!(
    YourProvider,
    "wasmcloud:contract",
    "your-world"
);

/// Implementation that you will contribute
struct YourProvider;

impl Trait for YourProvider {
  ...
}
```

Note that arguments after the second are similar to the options used by `wit-bindgen`, but the code generated is meant for use in a Rust binary, managed by a [`wasmcloud` host][wasmcloud-host].

For example, to build a provider for the [wasmCloud keyvalue WIT interface][wasmcloud-keyvalue]:

```rust
/// Generate bindings for a wasmCloud provider
wit_bindgen_wasmcloud::provider::host::generate!(
    MyKeyvalueProvider,
    "wasmcloud:keyvalue",
    "keyvalue"
);
```

> **Warning**
> You'll need to have the appropriate WIT interface file (ex. `keyvalue.wit`) in your crate root, at `<crate root>/wit/keyvalue.wit`

For more details on how the WIT ecosystem and `wit-bindgen`/`wasmtime::component::bindgen` work, see:

- [the `wit-bindgen` repository][wit-bindgen]
- [`wasmtime/crates/component-macro`][wasmtime-component-macro] respectively.

Note that after you generate bindings appropriate for your WIT, you must:

- follow the compiler to implement the appropriate traits
- write a `main.rs` that properly sets up your provider
- use the compiled binary for your provider on your wasmCloud lattice

[wit-bindgen]: https://github.com/bytecodealliance/wit-bindgen
[wasmtime-component-macro]: https://github.com/bytecodealliance/wasmtime/blob/main/crates/component-macro
[wasmcloud-capability-providers]: https://wasmcloud.com/docs/fundamentals/capabilities/
[wit]: https://github.com/WebAssembly/component-model/blob/main/design/mvp/WIT.md
[wasmtime-component-bindgen]: https://docs.rs/wasmtime/latest/wasmtime/component/macro.bindgen.html
[wasmcloud-keyvalue]: https://github.com/wasmCloud/interfaces/blob/main/wit/keyvalue.wit
[wasmcloud-host]: https://github.com/wasmCloud/wasmCloud
