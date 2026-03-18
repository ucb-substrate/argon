import * as fs from "fs";
import * as path from "path";

export function findWorkspaceRoot(
    startPath: string,
    existsSync: (candidate: string) => boolean = fs.existsSync
): string | undefined {
    let dir = path.dirname(startPath);

    while (true) {
        const candidate = path.join(dir, "lib.ar");
        if (existsSync(candidate)) {
            return dir;
        }

        const parent = path.dirname(dir);
        if (parent === dir) {
            return undefined;
        }
        dir = parent;
    }
}

export function buildServerCommand(argonRepoPath?: string): string {
    if (argonRepoPath && argonRepoPath.length > 0) {
        return path.join(argonRepoPath, "target", "release", "lang-server");
    }
    return "lang-server";
}
