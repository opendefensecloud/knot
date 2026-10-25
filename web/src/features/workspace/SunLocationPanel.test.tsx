import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { sunDay, type LatLon } from "../../lib/sun";
import { useUi } from "../../stores/ui";

import { SunLocationPanel } from "./SunLocationPanel";

const berlin = { lat: 52.52, lon: 13.4 };
const tromso = { lat: 69.65, lon: 18.96 };

const ui = () => useUi.getState();

/** Pins the clock. Only Date is faked, so React schedules as usual. */
function pinClock(iso: string) {
  vi.useFakeTimers({ toFake: ["Date"] });
  vi.setSystemTime(new Date(iso));
}

/** Makes the browser report `timeZone`, leaving the rest of the options real. */
function reportZone(timeZone: string) {
  const real = Intl.DateTimeFormat().resolvedOptions();
  vi.spyOn(Intl.DateTimeFormat.prototype, "resolvedOptions").mockReturnValue({ ...real, timeZone });
}

/** A clock time the way the panel writes it, in the browser's locale. */
const clockTime = (at: Date) => new Intl.DateTimeFormat(undefined, { timeStyle: "short" }).format(at);

/** `hour` o'clock on the pinned day, in the browser's local time. */
function today(hour: number): Date {
  const now = new Date();
  return new Date(now.getFullYear(), now.getMonth(), now.getDate(), hour);
}

/** The times line for `loc` on the pinned day, worked out the way the
 *  panel does: the solar day around local noon, in the browser's locale. */
function expectedTimes(loc: LatLon): string {
  const day = sunDay(today(12), loc);
  if (day.kind !== "normal") throw new Error(`expected sunrise and sunset, got ${day.kind}`);
  return `Light from ${clockTime(day.sunrise)} to ${clockTime(day.sunset)} today.`;
}

function openForm() {
  fireEvent.click(screen.getByTestId("sun-location-edit"));
  return {
    lat: screen.getByTestId("sun-lat"),
    lon: screen.getByTestId("sun-lon"),
  };
}

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.useRealTimers();
});

beforeEach(() => {
  localStorage.clear();
  // Drops the slots, mode and location earlier tests left in the store.
  ui().adoptStoredSkinPrefs();
  pinClock("2026-06-21T10:00:00Z");
});

describe("SunLocationPanel", () => {
  it("shows today's sunrise and sunset at a typed location", () => {
    ui().setSunLocation(berlin);
    render(<SunLocationPanel />);
    expect(screen.getByTestId("sun-times")).toHaveTextContent(expectedTimes(berlin));
    expect(screen.getByTestId("sun-location-source")).toHaveTextContent("Location set to 52.52° N, 13.40° E.");
    expect(screen.getByTestId("sun-location-edit")).toHaveTextContent("Change");
    expect(screen.getByTestId("sun-location-reset")).toHaveTextContent("Use time zone");
  });

  it("says when the sun doesn't set", () => {
    ui().setSunLocation(tromso);
    render(<SunLocationPanel />);
    expect(screen.getByTestId("sun-times")).toHaveTextContent("The sun doesn't set today, so knot stays light.");
  });

  it("says when the sun doesn't rise", () => {
    pinClock("2026-12-21T10:00:00Z");
    ui().setSunLocation(tromso);
    render(<SunLocationPanel />);
    expect(screen.getByTestId("sun-times")).toHaveTextContent("The sun doesn't rise today, so knot stays dark.");
  });

  it("starts the form at the location in use", () => {
    ui().setSunLocation(berlin);
    render(<SunLocationPanel />);
    const { lat, lon } = openForm();
    expect(screen.getByTestId("sun-location-form")).toBeInTheDocument();
    expect(screen.getByLabelText("Latitude")).toBe(lat);
    expect(screen.getByLabelText("Longitude")).toBe(lon);
    expect(lat).toHaveValue("52.52");
    expect(lon).toHaveValue("13.40");
    expect(lat).toHaveFocus();
  });

  it("rejects a latitude out of range and keeps the location", () => {
    ui().setSunLocation(berlin);
    render(<SunLocationPanel />);
    const { lat } = openForm();
    fireEvent.change(lat, { target: { value: "91" } });
    fireEvent.click(screen.getByTestId("sun-location-save"));
    const error = screen.getByRole("alert");
    expect(error).toBe(screen.getByTestId("sun-location-error"));
    expect(error).toHaveTextContent("Latitude must be a number from -90 to 90.");
    expect(lat).toHaveAttribute("aria-invalid", "true");
    expect(lat).toHaveAccessibleDescription("Latitude must be a number from -90 to 90.");
    expect(ui().sunLocation).toEqual(berlin);
    expect(screen.getByTestId("sun-location-form")).toBeInTheDocument();
  });

  it("rejects a longitude that isn't a number", () => {
    ui().setSunLocation(berlin);
    render(<SunLocationPanel />);
    const { lat, lon } = openForm();
    fireEvent.change(lon, { target: { value: "east" } });
    fireEvent.click(screen.getByTestId("sun-location-save"));
    expect(screen.getByTestId("sun-location-error")).toHaveTextContent("Longitude must be a number from -180 to 180.");
    expect(lat).toHaveAttribute("aria-invalid", "false");
    expect(lon).toHaveAttribute("aria-invalid", "true");
    expect(lon).toHaveFocus();
    expect(ui().sunLocation).toEqual(berlin);
  });

  it("marks every field that is wrong and focuses the first", () => {
    ui().setSunLocation(berlin);
    render(<SunLocationPanel />);
    const { lat, lon } = openForm();
    fireEvent.change(lat, { target: { value: "north" } });
    fireEvent.change(lon, { target: { value: "200" } });
    const save = screen.getByTestId("sun-location-save");
    save.focus();
    fireEvent.click(save);
    const error = screen.getByTestId("sun-location-error");
    expect(error).toHaveTextContent("Latitude must be a number from -90 to 90.");
    expect(error).toHaveTextContent("Longitude must be a number from -180 to 180.");
    expect(lat).toHaveAccessibleDescription("Latitude must be a number from -90 to 90.");
    expect(lon).toHaveAccessibleDescription("Longitude must be a number from -180 to 180.");
    expect(lat).toHaveFocus();

    // The same failure again moves focus back, so it is read out again.
    save.focus();
    fireEvent.click(save);
    expect(lat).toHaveFocus();
    expect(ui().sunLocation).toEqual(berlin);
  });

  it("saves a typed location and closes the form", () => {
    ui().setSunLocation(berlin);
    render(<SunLocationPanel />);
    const { lat, lon } = openForm();
    fireEvent.change(lat, { target: { value: "48,14" } });
    fireEvent.change(lon, { target: { value: "11.58" } });
    fireEvent.click(screen.getByTestId("sun-location-save"));
    expect(ui().sunLocation).toEqual({ lat: 48.14, lon: 11.58 });
    expect(screen.queryByTestId("sun-location-form")).toBeNull();
    expect(screen.getByTestId("sun-location-source")).toHaveTextContent("Location set to 48.14° N, 11.58° E.");
    expect(screen.getByTestId("sun-location-edit")).toHaveFocus();
  });

  it("describes the edit button with the times and the location", () => {
    ui().setSunLocation(berlin);
    render(<SunLocationPanel />);
    const described = screen.getByTestId("sun-location-edit").getAttribute("aria-describedby")?.split(" ");
    expect(described).toEqual([screen.getByTestId("sun-times").id, screen.getByTestId("sun-location-source").id]);
    expect(screen.getByTestId("sun-location-edit")).toHaveAccessibleDescription(
      `${expectedTimes(berlin)} Location set to 52.52° N, 13.40° E.`,
    );
  });

  it("closes the form on Cancel without saving", () => {
    ui().setSunLocation(berlin);
    render(<SunLocationPanel />);
    const { lat } = openForm();
    fireEvent.change(lat, { target: { value: "10" } });
    fireEvent.click(screen.getByTestId("sun-location-cancel"));
    expect(screen.queryByTestId("sun-location-form")).toBeNull();
    expect(ui().sunLocation).toEqual(berlin);
    // Focus goes back where the form was opened from.
    expect(screen.getByTestId("sun-location-edit")).toHaveFocus();
  });

  it("goes back to the time zone's location", () => {
    reportZone("Europe/Berlin");
    ui().setSunLocation(berlin);
    render(<SunLocationPanel />);
    fireEvent.click(screen.getByTestId("sun-location-reset"));
    expect(ui().sunLocation).toBeNull();
    expect(screen.getByTestId("sun-location-source")).toHaveTextContent("Location from your time zone, Europe/Berlin.");
    expect(screen.queryByTestId("sun-location-reset")).toBeNull();
    expect(screen.getByTestId("sun-location-edit")).toHaveTextContent("Set location");
    expect(screen.getByTestId("sun-location-edit")).toHaveFocus();
    // The form starts at the zone's city, which is what is in use now.
    const { lat, lon } = openForm();
    expect(lat).toHaveValue("52.50");
    expect(lon).toHaveValue("13.37");
  });

  it("admits a time zone it can't place", () => {
    reportZone("Etc/UTC");
    render(<SunLocationPanel />);
    expect(screen.getByTestId("sun-location-source")).toHaveTextContent(
      `Your time zone (Etc/UTC) has no known location, so knot assumes daylight from about ${clockTime(today(6))} ` +
        `to ${clockTime(today(18))}. Set a location for real sunrise and sunset times.`,
    );
    expect(screen.getByTestId("sun-location-edit")).toHaveTextContent("Set location");
  });

  it("calls a zone the browser doesn't report unknown", () => {
    reportZone("");
    render(<SunLocationPanel />);
    expect(screen.getByTestId("sun-location-source")).toHaveTextContent(/^Your time zone \(unknown\) has no known location/);
  });
});
