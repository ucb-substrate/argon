const assert = require("assert");
const fs = require("fs");
const os = require("os");
const path = require("path");

const { buildServerCommand, findWorkspaceRoot } = require("../out/helpers");

describe("Argon VS Code helpers", () => {
  it("finds the nearest workspace root containing lib.ar", () => {
    const tempRoot = fs.mkdtempSync(path.join(os.tmpdir(), "argon-vscode-test-"));
    const workspaceRoot = path.join(tempRoot, "workspace");
    const nested = path.join(workspaceRoot, "src", "nested");

    fs.mkdirSync(nested, { recursive: true });
    fs.writeFileSync(path.join(workspaceRoot, "lib.ar"), "cell top() {}\n");
    fs.writeFileSync(path.join(nested, "test.ar"), "cell nested() {}\n");

    assert.strictEqual(
      findWorkspaceRoot(path.join(nested, "test.ar")),
      workspaceRoot
    );
  });

  it("returns undefined when no enclosing lib.ar exists", () => {
    const tempRoot = fs.mkdtempSync(path.join(os.tmpdir(), "argon-vscode-test-"));
    const sourceFile = path.join(tempRoot, "orphan.ar");

    fs.writeFileSync(sourceFile, "cell top() {}\n");

    assert.strictEqual(findWorkspaceRoot(sourceFile), undefined);
  });

  it("builds a repo-local lang-server command when configured", () => {
    assert.strictEqual(
      buildServerCommand("/tmp/argon"),
      path.join("/tmp/argon", "target", "release", "lang-server")
    );
  });

  it("falls back to lang-server on PATH when no repo path is configured", () => {
    assert.strictEqual(buildServerCommand(undefined), "lang-server");
    assert.strictEqual(buildServerCommand(""), "lang-server");
  });
});
