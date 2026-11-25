import { invoke } from "@tauri-apps/api/core";
import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import App from "./App";
import { app, base } from "./fixtures";

const mock = vi.hoisted(() => ({ phase: "IDLE" as string, setup: false }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn((name: string) => Promise.resolve(name === "get_display_state" ? { view: "screen", previewOnly: false, lastError: null } : undefined)) }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(() => Promise.resolve(() => {})) }));
vi.mock("./useSnapshot", () => ({ useSnapshot: () => ({ ...base, phase: mock.phase }) }));
vi.mock("./useAppState", () => ({ useAppState: () => ({ ...app, routine: null, setup: mock.setup }) }));

describe("launcher", () => {
  it("is two illustrated actions and a cog, nothing else (UI-01)", () => {
    Object.assign(mock, { phase: "IDLE", setup: false });
    const { container } = render(<App forceRole="main" />);
    expect(screen.getByRole("button", { name: /Start workout/ })).toHaveTextContent("Next: Lower A");
    expect(screen.getByRole("button", { name: /Configure workout/ })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Settings" })).toBeInTheDocument();
    expect(container.querySelectorAll("button")).toHaveLength(3);
    expect(container.querySelector("svg, img, canvas[aria-label*=chart]")).not.toBeNull();
  });
  it("Start prepares the monitor; a live session reads Resume", () => {
    Object.assign(mock, { phase: "IDLE", setup: false });
    const { unmount } = render(<App forceRole="main" />);
    fireEvent.click(screen.getByRole("button", { name: /Start workout/ }));
    expect(invoke).toHaveBeenCalledWith("prepare_workout");
    unmount();
    Object.assign(mock, { phase: "CODING" });
    render(<App forceRole="main" />);
    expect(screen.getByRole("button", { name: /Resume workout/ })).toBeInTheDocument();
  });
  it("setup can start the session from the work desktop", () => {
    Object.assign(mock, { phase: "IDLE", setup: true });
    render(<App forceRole="main" />);
    fireEvent.click(screen.getByRole("button", { name: "Start session" }));
    expect(invoke).toHaveBeenCalledWith("start_session");
  });
  it("routes the gym and prompt windows", () => {
    Object.assign(mock, { phase: "CODING", setup: false });
    const { unmount } = render(<App forceRole="gym" />);
    expect(screen.getByText("CODE")).toBeInTheDocument();
    unmount();
    render(<App forceRole="prompt" />);
    expect(screen.getByText("Nothing due right now.")).toBeInTheDocument();
  });
});
