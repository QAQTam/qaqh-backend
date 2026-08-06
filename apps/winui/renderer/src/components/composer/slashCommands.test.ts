import { describe, expect, it } from "vitest";
import { matchSlashCommands } from "./slashCommands";

describe("matchSlashCommands", () => {
  it("opens only for a slash at the start of the composer", () => {
    expect(matchSlashCommands("model")).toEqual([]);
    expect(matchSlashCommands(" /model")).toEqual([]);
    expect(matchSlashCommands("/").length).toBeGreaterThan(0);
  });

  it("filters by command name and keeps the selected command text stable", () => {
    expect(matchSlashCommands("/mod").map(item => item.command)).toEqual(["/model"]);
    expect(matchSlashCommands("/强").map(item => item.command)).toEqual(["/effort"]);
  });
});
