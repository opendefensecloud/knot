/**
 * The suggestion composer: what it sends for the selection the store holds,
 * and how it keeps the replacement on one line.
 */

import React from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useUi, type PendingSuggestion } from "../../stores/ui";
import { createErrorMessage, SuggestionComposer } from "./SuggestionComposer";

const api = vi.hoisted(() => ({
  list: vi.fn(),
  create: vi.fn(),
  accept: vi.fn(),
  reject: vi.fn(),
  withdraw: vi.fn(),
  reply: vi.fn(),
}));
vi.mock("../../lib/suggestions.api", () => ({ suggestionsApi: api }));

const REPLACE: PendingSuggestion = {
  kind: "replace",
  positionY: "AAcEAA==",
  positionYEnd: "AAcFQQ==",
  originalText: "charlie",
  contextBefore: "alpha bravo ",
};

const INSERT: PendingSuggestion = {
  kind: "insert",
  positionY: "AAcEQQ==",
  positionYEnd: "AAcEQQ==",
  originalText: "",
  contextBefore: "alpha bravo",
};

function renderComposer(pending: PendingSuggestion) {
  useUi.setState({ pendingSuggestion: pending });
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const invalidate = vi.spyOn(qc, "invalidateQueries");
  render(
    <QueryClientProvider client={qc}>
      <SuggestionComposer docId="d1" />
    </QueryClientProvider>,
  );
  return { invalidate };
}

beforeEach(() => {
  api.create.mockReset();
  api.create.mockResolvedValue({ ok: {} as never });
});

afterEach(() => {
  cleanup();
  useUi.setState({ pendingSuggestion: null, toasts: [] });
});

describe("SuggestionComposer", () => {
  it("renders the copy and prefills the replacement with the selection", () => {
    renderComposer(REPLACE);
    const composer = screen.getByTestId("suggestion-composer");
    expect(composer).toHaveTextContent("Suggest a change");
    expect(composer).toHaveTextContent("Current text");
    expect(composer).toHaveTextContent("Replace with");
    expect(composer).toHaveTextContent("Why? (optional)");
    expect(composer).toHaveTextContent("The page stays as it is until an owner accepts.");
    expect(screen.getByTestId("suggestion-composer-replace")).toHaveValue("charlie");
    expect(screen.getByTestId("suggestion-composer-submit")).toHaveTextContent("Send suggestion");
    expect(screen.getByTestId("suggestion-composer-cancel")).toHaveTextContent("Cancel");
  });

  it("composer_derives_delete_when_replacement_empty", async () => {
    const { invalidate } = renderComposer(REPLACE);
    fireEvent.change(screen.getByTestId("suggestion-composer-replace"), { target: { value: "" } });
    fireEvent.change(screen.getByTestId("suggestion-composer-reason"), { target: { value: "redundant" } });
    fireEvent.click(screen.getByTestId("suggestion-composer-submit"));

    await waitFor(() => expect(api.create).toHaveBeenCalledTimes(1));
    expect(api.create).toHaveBeenCalledWith("d1", {
      kind: "delete",
      position_y: REPLACE.positionY,
      position_y_end: REPLACE.positionYEnd,
      original_text: "charlie",
      context_before: "alpha bravo ",
      proposed_text: "",
      reason: "redundant",
    });
    await waitFor(() => expect(useUi.getState().pendingSuggestion).toBeNull());
    expect(invalidate).toHaveBeenCalledWith({ queryKey: ["suggestions", "d1"] });
  });

  it("derives replace for new text and insert for a caret", async () => {
    renderComposer(REPLACE);
    fireEvent.change(screen.getByTestId("suggestion-composer-replace"), { target: { value: "charlotte" } });
    fireEvent.click(screen.getByTestId("suggestion-composer-submit"));
    await waitFor(() => expect(api.create).toHaveBeenCalledTimes(1));
    expect(api.create.mock.calls[0]?.[1]).toMatchObject({
      kind: "replace",
      proposed_text: "charlotte",
    });
    // No reason typed: none is sent.
    expect(api.create.mock.calls[0]?.[1]).not.toHaveProperty("reason");
    cleanup();

    renderComposer(INSERT);
    // An insertion has no current text to show.
    expect(screen.getByTestId("suggestion-composer")).not.toHaveTextContent("Current text");
    expect(screen.getByTestId("suggestion-composer")).toHaveTextContent("Insert");
    fireEvent.change(screen.getByTestId("suggestion-composer-replace"), { target: { value: " delta" } });
    fireEvent.click(screen.getByTestId("suggestion-composer-submit"));
    await waitFor(() => expect(api.create).toHaveBeenCalledTimes(2));
    expect(api.create.mock.calls[1]?.[1]).toMatchObject({
      kind: "insert",
      original_text: "",
      proposed_text: " delta",
    });
  });

  it("composer_submit_disabled_when_unchanged", () => {
    renderComposer(REPLACE);
    const submit = screen.getByTestId("suggestion-composer-submit");
    expect(submit).toBeDisabled();
    fireEvent.change(screen.getByTestId("suggestion-composer-replace"), { target: { value: "charlotte" } });
    expect(submit).toBeEnabled();
    fireEvent.change(screen.getByTestId("suggestion-composer-replace"), { target: { value: "charlie" } });
    expect(submit).toBeDisabled();
    cleanup();

    // An insertion with nothing typed is unchanged too.
    renderComposer(INSERT);
    expect(screen.getByTestId("suggestion-composer-submit")).toBeDisabled();
  });

  it("composer_turns_pasted_newlines_into_spaces", () => {
    renderComposer(REPLACE);
    const input = screen.getByTestId<HTMLInputElement>("suggestion-composer-replace");
    input.setSelectionRange(0, input.value.length);
    fireEvent.paste(input, {
      clipboardData: { getData: (type: string) => (type === "text/plain" ? "one\ntwo\r\nthree" : "") },
    });
    expect(input).toHaveValue("one two three");
  });

  it("cancel clears the pending suggestion without sending", () => {
    renderComposer(REPLACE);
    fireEvent.click(screen.getByTestId("suggestion-composer-cancel"));
    expect(useUi.getState().pendingSuggestion).toBeNull();
    expect(api.create).not.toHaveBeenCalled();
  });

  it("keeps the composer open when the server refuses", async () => {
    api.create.mockResolvedValue({
      error: { code: "suggestion.too_long", message: "too long", details: {}, status: 422 },
    });
    renderComposer(REPLACE);
    fireEvent.change(screen.getByTestId("suggestion-composer-replace"), { target: { value: "charlotte" } });
    fireEvent.click(screen.getByTestId("suggestion-composer-submit"));
    await waitFor(() => expect(api.create).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(screen.getByTestId("suggestion-composer-submit")).toBeEnabled());
    expect(useUi.getState().pendingSuggestion).toEqual(REPLACE);
    expect(useUi.getState().toasts.map((t) => t.text)).toEqual(["That suggestion is too long"]);
  });

  it("names_the_refusal_in_its_toast", () => {
    expect(createErrorMessage("suggestion.too_long")).toBe("That suggestion is too long");
    expect(createErrorMessage("suggestion.invalid_anchor")).toBe(
      "Couldn't place the suggestion. Select the text again.",
    );
    expect(createErrorMessage("doc.archived")).toBe("This page is archived and can't take suggestions");
    expect(createErrorMessage("http_error")).toBe("Couldn't send suggestion");
  });
});
