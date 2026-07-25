module.exports = async ({ github, context, core }) => {
  const fs = require("fs");

  const PREDEFINED_NAMES = ["ci", "docs"];

  function escapeRegExp(value) {
    return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  }

  function toIgnoredDirectorySet(gitignoreContent) {
    const ignored = new Set();
    const lines = gitignoreContent.split(/\r?\n/);

    for (const line of lines) {
      const trimmed = line.trim();

      if (
        !trimmed ||
        trimmed.startsWith("#") ||
        trimmed.startsWith("!") ||
        trimmed.includes("*") ||
        trimmed.includes("?")
      ) {
        continue;
      }

      const normalized = trimmed.replace(/^\//, "").replace(/\/$/, "");

      if (normalized && !normalized.includes("/")) {
        ignored.add(normalized);
      }
    }

    return ignored;
  }

  async function readIgnoredDirectories() {
    try {
      const content = await fs.promises.readFile(".gitignore", "utf8");
      return toIgnoredDirectorySet(content);
    } catch {
      return new Set();
    }
  }

  async function getDirectoryNames() {
    const ignoredDirectories = await readIgnoredDirectories();
    const entries = await fs.promises.readdir(".", { withFileTypes: true });

    return entries
      .filter((entry) => entry.isDirectory())
      .map((entry) => entry.name)
      .filter((name) => !name.startsWith("."))
      .filter((name) => !ignoredDirectories.has(name));
  }

  async function run() {
    try {
      const directoryNames = await getDirectoryNames();
      const allowedNames = [
        ...new Set([...PREDEFINED_NAMES, ...directoryNames]),
      ].sort();
      const source = allowedNames.map((name) => escapeRegExp(name)).join("|");
      const regex = new RegExp(`^\[(${source})\] .+ #\\d+$`);
      const title = context.payload.pull_request.title;

      if (!regex.test(title)) {
        core.setFailed(
          `PR title "${title}" failed to pass regex - ${regex}. Correct example: [${allowedNames[0]}] description #1`,
        );
      }
    } catch (error) {
      core.setFailed(`Error validating PR title: ${error}`);
    }
  }

  await run();
};
