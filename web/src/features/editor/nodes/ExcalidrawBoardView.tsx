import { NodeViewWrapper, type ReactNodeViewProps } from "@tiptap/react";
import { useQuery } from "@tanstack/react-query";
import { useState } from "react";

import { boardsApi } from "../../../lib/boards.api";
import { sanitizeSvg } from "../../../lib/sanitize";
import { ExcalidrawModal } from "../../boards/ExcalidrawModal";

export function ExcalidrawBoardView({ node, updateAttributes }: ReactNodeViewProps) {
  const boardId = node.attrs.board_id as string;
  const label = (node.attrs.label as string | null) ?? null;
  const displayLabel = label && label.trim().length > 0 ? label : "Diagram";
  const [modalOpen, setModalOpen] = useState(false);
  const svg = useQuery({
    queryKey: ["board-svg", boardId],
    queryFn: () => boardsApi.getSvg(boardId),
    staleTime: 5_000,
    enabled: !!boardId,
  });
  return (
    <NodeViewWrapper
      as="div"
      data-testid="excalidraw-board"
      data-excalidraw-board="true"
      className="my-3 rounded-md border border-border bg-surface overflow-hidden"
    >
      <div className="px-3 py-1.5 border-b border-border bg-muted/40 flex items-center">
        <span
          className="text-[11px] font-semibold uppercase tracking-wider text-fg-muted"
          data-testid="excalidraw-board-label"
        >
          {displayLabel}
        </span>
        <button
          type="button"
          className="ml-auto text-xs text-fg-muted hover:text-fg"
          onClick={() => setModalOpen(true)}
          data-testid="excalidraw-board-open"
        >
          Open
        </button>
      </div>
      <button
        type="button"
        className="block w-full p-3 text-left"
        onClick={() => setModalOpen(true)}
      >
        {svg.data && "ok" in svg.data ? (
          <div
            className="flex justify-center [&_svg]:max-w-full [&_svg]:h-auto"
            dangerouslySetInnerHTML={{ __html: sanitizeSvg(svg.data.ok) }}
          />
        ) : (
          <div className="h-40 grid place-items-center text-fg-muted text-sm">
            No preview yet — click to draw
          </div>
        )}
      </button>
      {modalOpen && (
        <ExcalidrawModal
          boardId={boardId}
          label={label}
          onLabelChange={(next) =>
            updateAttributes({ label: next.trim().length > 0 ? next : null })
          }
          onClose={() => setModalOpen(false)}
        />
      )}
    </NodeViewWrapper>
  );
}
