import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import App from "./App";

vi.mock("./ipc", () => ({
  commands: {
    appInfo: async () => ({
      status: "ok",
      data: { version: "9.9.9", data_dir: "/tmp/murmur" },
    }),
  },
  unwrap: (result: { data: unknown }) => result.data,
}));

afterEach(cleanup);

describe("App", () => {
  it("shows the app version from app_info", async () => {
    render(<App />);
    expect(await screen.findByText("Murmur v9.9.9")).toBeTruthy();
    expect(screen.getByText("Data: /tmp/murmur")).toBeTruthy();
  });
});
