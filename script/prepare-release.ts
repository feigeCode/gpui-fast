#!/usr/bin/env bun
// Generate registry-only release manifests without touching upstream files.
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { extname, relative, resolve, sep } from "node:path";
import { parseArgs } from "node:util";
import { DEST, ROOT, fastName, readConfig, readToml, run, type Manifest, type ReleaseConfig } from "./release-common";

export function versionFromTag(tag: string): string {
  const match = /^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$/.exec(tag);
  if (!match || match[4]?.split(".").some(part => /^0\d+$/.test(part))) {
    throw new Error("Tag must have the form v0.1.0 or v0.1.0-rc.1");
  }
  if (Number(match[1]) === 0 && Number(match[2]) === 0) {
    throw new Error("The first release version is 0.1.0");
  }
  return tag.slice(1);
}

function snapshotName(name: string): string {
  // gpui_util is distinct from Zed's util crate.
  return "gpui-pre-" + name.replace(/^gpui_/, "").replaceAll("_", "-");
}

export function dependency(key: string, input: any, workspace: Manifest, config: ReleaseConfig, version: string, crate?: string): any {
  if (typeof input === "string") return input;
  let value = structuredClone(input);
  if (value.workspace) {
    delete value.workspace;
    let inherited = structuredClone(workspace.dependencies[key]);
    if (typeof inherited === "string") inherited = { version: inherited };
    const features = [...new Set([...(inherited.features ?? []), ...(value.features ?? [])])];
    delete value.features;
    value = { ...inherited, ...value };
    if (features.length) value.features = features;
  }
  if ("path" in value) {
    delete value.path;
    Object.assign(value, config.crates.includes(key)
      ? { package: fastName(key), version: "^" + version, path: "../" + key }
      : { package: snapshotName(key), version: "=" + config.snapshot_version });
    // A crate listed as "parent/leaf" (e.g. refineable/derive_refineable) is
    // depended on by its bare leaf key; remap that to the sibling fast crate.
    if (value.package === snapshotName(key) && crate && config.crates.includes(crate + "/" + key)) {
      value = { package: fastName(key), version: "^" + version, path: "../" + crate + "/" + key };
    }
  }
  if ("git" in value) {
    if (!value.version) {
      if (key !== "proptest" && !(crate === "http_client" && key === "async-tar")) throw new Error(`${key}: Git dependency has no registry version`);
      value.version = "1";
    }
    for (const field of ["git", "rev", "branch", "tag"]) delete value[field];
  }
  return value;
}

export function prepare(config: ReleaseConfig, version: string, dest = DEST): void {
  versionFromTag("v" + version);
  const workspace = readToml(resolve(ROOT, "Cargo.toml")).workspace;
  rmSync(dest, { recursive: true, force: true });
  mkdirSync(dest, { recursive: true });
  // gpui-pre compatibility shims: same names and versions as the crates.io
  // gpui-pre snapshots GPUI Kit pins, forwarding to the sibling gpui-fast
  // crates in this release. Consumers patch these over crates-io and the
  // whole kit graph compiles against this repository's gpui.
  const compat: Record<string, { forwards: string; features: Record<string, string[]> }> = {
    "gpui-pre": { forwards: "gpui", features: {
      bench: ["gpui_fast/bench"], "bench-support": ["gpui_fast/bench-support"],
      inspector: ["gpui_fast/inspector"], "leak-detection": ["gpui_fast/leak-detection"],
      profiler: ["gpui_fast/profiler"], "screen-capture": ["gpui_fast/screen-capture"],
      stacker: ["gpui_fast/stacker"], "test-support": ["gpui_fast/test-support"],
      wayland: ["gpui_fast/wayland"], "windows-manifest": ["gpui_fast/windows-manifest"],
      x11: ["gpui_fast/x11"],
    } },
    "gpui-pre-macros": { forwards: "gpui_macros", features: { inspector: ["gpui_fast/inspector"] } },
    "gpui-pre-platform": { forwards: "gpui_platform", features: {
      "bench-support": ["gpui_fast/bench-support"], "font-kit": ["gpui_fast/font-kit"],
      runtime_shaders: ["gpui_fast/runtime_shaders"], "screen-capture": ["gpui_fast/screen-capture"],
      "test-support": ["gpui_fast/test-support"], wayland: ["gpui_fast/wayland"], x11: ["gpui_fast/x11"],
    } },
    "gpui-pre-sum-tree": { forwards: "sum_tree", features: { "test-support": ["gpui_fast/test-support"] } },
    "gpui-pre-reqwest-client": { forwards: "reqwest_client", features: {} },
    "gpui-pre-util": { forwards: "gpui_util", features: {} },
    // Platform and remaining snapshot crates: cover every gpui-pre-* name the
    // fork published so no crates.io gpui-pre-* crate (whose internal =0.3.8
    // pins would conflict with the 0.3.126 shims) is ever selected.
    "gpui-pre-apple": { forwards: "gpui_apple", features: {} },
    "gpui-pre-linux": { forwards: "gpui_linux", features: {} },
    "gpui-pre-macos": { forwards: "gpui_macos", features: {} },
    "gpui-pre-windows": { forwards: "gpui_windows", features: {} },
    "gpui-pre-wgpu": { forwards: "gpui_wgpu", features: {} },
    "gpui-pre-web": { forwards: "gpui_web", features: {} },
    "gpui-pre-bench-metrics": { forwards: "bench_metrics", features: {} },
    "gpui-pre-collections": { forwards: "collections", features: {} },
    "gpui-pre-derive-refineable": { forwards: "refineable/derive_refineable", features: {} },
    "gpui-pre-refineable": { forwards: "refineable", features: {} },
    "gpui-pre-http-client": { forwards: "http_client", features: {} },
    "gpui-pre-http-client-tls": { forwards: "http_client_tls", features: {} },
    "gpui-pre-scheduler": { forwards: "scheduler", features: {} },
    "gpui-pre-shared-string": { forwards: "gpui_shared_string", features: {} },
    "gpui-pre-zlog": { forwards: "zlog", features: {} },
    "gpui-pre-ztracing": { forwards: "ztracing", features: {} },
    "gpui-pre-ztracing-macro": { forwards: "ztracing_macro", features: {} },
  };
  writeFileSync(resolve(dest, "Cargo.toml"), Bun.TOML.stringify({
    workspace: { resolver: "2", members: [...config.crates, ...Object.keys(compat)] },
  }));
  cpSync(resolve(ROOT, "rust-toolchain.toml"), resolve(dest, "rust-toolchain.toml"));

  for (const name of config.crates) {
    const source = resolve(ROOT, "crates", name);
    const target = resolve(dest, name);
    const leaf = name.split("/").pop()!; // subdir crates publish under their leaf name
    cpSync(source, target, { recursive: true });
    const manifest = readToml(resolve(source, "Cargo.toml"));
    const pkg = manifest.package;
    Object.assign(pkg, {
      name: fastName(leaf), version, publish: ["crates-io"],
      edition: workspace.package.edition, repository: config.repository,
      homepage: config.homepage, readme: "README.md", license: "Apache-2.0",
      documentation: `https://docs.rs/${fastName(leaf)}/${version}/${leaf}/`,
      description: `${leaf}: GPUI with gpui-fast rendering and layout optimizations`,
      autoexamples: false, autobenches: false, autotests: false,
      authors: [...new Set([...(pkg.authors ?? []), "Longbridge"])],
      include: ["/src/**", "/resources/**", "/build.rs", "/Cargo.toml", "/README.md", "/LICENSE", "/NOTICE"],
    });
    delete pkg.metadata;
    manifest.lib.name = leaf;
    for (const field of ["lints", "example", "bench", "test", "dev-dependencies"]) delete manifest[field];
    // Published crates have no workspace to inherit lints from; keep its rustc
    // lints so upstream's cfgs, such as `rust_analyzer`, stay allowed.
    manifest.lints = { rust: workspace.lints.rust };
    for (const group of [manifest, ...Object.values(manifest.target ?? {})] as Manifest[]) {
      delete group["dev-dependencies"];
      for (const section of ["dependencies", "build-dependencies"]) {
        if (group[section]) group[section] = Object.fromEntries(
          Object.entries(group[section]).map(([key, value]) => [key, dependency(key, value, workspace, config, version, name)]),
        );
      }
    }
    // zed-scap allows windows-capture 1.5, whose settings API is incompatible.
    // A direct registry constraint also reaches consumers without a root patch.
    if (name === "gpui") {
      manifest.target ??= {};
      const windows = manifest.target['cfg(target_os = "windows")'] ??= {};
      windows.dependencies ??= {};
      windows.dependencies["windows-capture"] = {
        version: config.windows_capture_version, optional: true,
      };
      manifest.features["screen-capture"].push("dep:windows-capture");
    }
    // http_client's `github-download` pulls a git-only async-tar the registry
    // snapshot cannot express; nothing enables it, so drop it (mirrors the
    // gpui-pre snapshot, which ships without it).
    if (name === "http_client" && manifest.features["github-download"]) {
      for (const dep of manifest.features["github-download"]) {
        if (dep.startsWith("dep:")) delete manifest.dependencies[dep.slice(4)];
      }
      delete manifest.features["github-download"];
      for (const group of [manifest, ...Object.values(manifest.target ?? {})] as Manifest[]) {
        for (const section of ["dependencies", "build-dependencies"]) {
          if (group[section]) {
            delete group[section]["async-tar"];
            delete group[section]["util"];
          }
        }
      }
    }
    // gpui's `util_macros` is only used inside #[cfg(test)] (the `perf`
    // attribute); snapshots drop tests, so drop the dependency too. Its
    // `perf` dependency lives in tooling/, not crates/, and stays out.
    if (name === "gpui") delete manifest.dependencies["util_macros"];
    writeFileSync(resolve(target, "Cargo.toml"),
      "# Modified by Longbridge for gpui-fast publication.\n" + Bun.TOML.stringify(manifest));
    cpSync(resolve(ROOT, "LICENSE"), resolve(target, "LICENSE"));
    const notices = [resolve(ROOT, "NOTICE"), resolve(source, "NOTICE")]
      .filter(existsSync).map(path => readFileSync(path, "utf8"));
    if (notices.length) writeFileSync(resolve(target, "NOTICE"), notices.join("\n\n"));
    writeFileSync(resolve(target, "README.md"),
      `# ${fastName(name)}\n\nGPUI with gpui-fast rendering and layout optimizations.\n\n`
      + `Source and documentation: [${config.repository}](${config.repository}).\n\n`
      + "Forked from Zed's GPUI; distributed under Apache-2.0.\n\n"
      + `Use the core as \`gpui = { package = "gpui-fast", version = "${version}" }\` to preserve paths emitted by its macros.\n`);
  }

  // cbindgen must read bundled inputs rather than a sibling registry package.
  const apple = resolve(dest, "gpui_apple");
  const bundled = resolve(apple, "src/fast/release_gpui/src");
  mkdirSync(bundled, { recursive: true });
  for (const file of ["scene.rs", "geometry.rs", "color.rs", "window.rs", "platform.rs"]) {
    cpSync(resolve(ROOT, "crates/gpui/src", file), resolve(bundled, file));
  }
  for (const directory of ["window", "platform"]) {
    cpSync(resolve(ROOT, "crates/gpui/src", directory), resolve(bundled, directory), { recursive: true });
  }
  const build = resolve(apple, "build.rs");
  const original = readFileSync(build, "utf8");
  const needle = '.join("../gpui")';
  if (original.split(needle).length !== 2) {
    throw new Error("gpui_apple build script changed; review bundled cbindgen inputs");
  }
  writeFileSync(build, "// Modified for gpui-fast releases: bundle cbindgen inputs inside this package.\n"
    + original.replace(needle, '.join("src/fast/release_gpui")'));

  // Apache-2.0 section 4(b) requires change notices in distributed copies.
  for (const [name, spec] of Object.entries(compat)) {
    const target = resolve(dest, name);
    mkdirSync(target, { recursive: true });
    const leafName = spec.forwards.split("/").pop()!;
    const fastPackage = spec.forwards.includes("/")
      ? leafName.replaceAll("_", "-")
      : fastName(spec.forwards);
    // `default` only forwards when the target crate has a default feature to
    // receive it (leaf crates like gpui_util have no feature table at all).
    const featureTable: Record<string, string[]> = { ...spec.features };
    if (spec.forwards === "gpui" || spec.forwards === "gpui_platform") {
      featureTable.default = ["gpui_fast/default"];
    }
    if (!Object.keys(featureTable).length) featureTable.default = [];
    writeFileSync(resolve(target, "Cargo.toml"), "# Modified by Longbridge for gpui-fast publication.\n" + Bun.TOML.stringify({
      package: {
        name, version: "0.3.126", license: "Apache-2.0", edition: workspace.package.edition,
        description: `${fastPackage} under the gpui-pre name GPUI Kit pins; forwards to it.`,
        repository: config.repository,
      },
      lib: { name: leafName, path: "lib.rs", doctest: false },
      features: featureTable,
      dependencies: { gpui_fast: { package: fastPackage, path: "../" + spec.forwards, "default-features": false } },
    } satisfies Record<string, unknown> as unknown as Manifest));
    writeFileSync(resolve(target, "lib.rs"),
      `//! ${fastPackage}, standing in for ${name}; see compat/README.md in the source repository.\n\npub use gpui_fast::*;\n`);
  }

  const upstream = readToml(resolve(ROOT, "UPSTREAM"));
  const changed = run([
    "git", "diff", "--name-only", upstream.import_commit, "--", ...config.crates.map(name => "crates/" + name),
  ], true).trim().split(/\r?\n/).filter(Boolean);
  for (const filename of changed) {
    const parts = relative(resolve(ROOT, "crates"), resolve(ROOT, filename)).split(sep);
    const distributed = [resolve(dest, ...parts)];
    if (parts[0] === "gpui" && parts[1] === "src") distributed.push(resolve(bundled, ...parts.slice(2)));
    for (const file of distributed) {
      if (existsSync(file) && statSync(file).isFile() && [".rs", ".metal", ".wgsl", ".hlsl"].includes(extname(file))) {
        writeFileSync(file, "// Modified by Longbridge for gpui-fast.\n" + readFileSync(file, "utf8"));
      }
    }
  }
  console.log(`Prepared ${config.crates.length} crates at ${dest}, version ${version}`);
}

if (import.meta.main) {
  try {
    const { values } = parseArgs({ args: Bun.argv.slice(2), options: { tag: { type: "string" }, help: { type: "boolean" } } });
    if (values.help) console.log("Usage: bun script/prepare-release.ts --tag v0.1.0");
    else {
      if (!values.tag) throw new Error("--tag is required (first release: v0.1.0)");
      prepare(readConfig(), versionFromTag(values.tag));
    }
  } catch (error) {
    console.error(error instanceof Error ? error.message : error);
    process.exitCode = 1;
  }
}
