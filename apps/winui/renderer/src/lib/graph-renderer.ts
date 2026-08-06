/**
 * G6 v5 graph renderer — creates/destroys G6 instances on DOM placeholders.
 *
 * Works with raw DOM (no SolidJS component layer) to integrate cleanly
 * with MarkdownBody's innerHTML-based rendering pipeline.
 */

import { Graph as G6Graph } from "@antv/g6";
import type { GraphData } from "@antv/g6";
import type { G6GraphData, G6TreeNode, GraphDSLType } from "./graph-dsl";

// ── Placeholder protocol ──

/** Data attributes set on placeholder divs by MarkdownBody's renderer.code */
export const ATTR_TYPE = "data-g6-type"; // "mindmap" | "graph"
export const ATTR_RAW = "data-g6-raw"; // URL-encoded raw DSL text
export const PLACEHOLDER_CLASS = "g6-placeholder";

const PLACEHOLDER_SELECTOR = `.${PLACEHOLDER_CLASS}[${ATTR_TYPE}][${ATTR_RAW}]`;

// ── Instance registry ──

interface GraphInstance {
  graph: G6Graph;
  container: HTMLElement;
}

/** Track live G6 graphs so we can destroy orphans. */
const liveGraphs = new Map<HTMLElement, GraphInstance>();

// ── Mindmap: G6TreeNode → GraphData (flatten for G6) ──

function flattenTree(
  node: G6TreeNode,
  parentId?: string,
): { nodes: GraphData["nodes"]; edges: GraphData["edges"] } {
  const nodes: NonNullable<GraphData["nodes"]> = [
    {
      id: node.id,
      data: { label: node.data.label },
    },
  ];
  const edges: NonNullable<GraphData["edges"]> = [];

  if (parentId !== undefined) {
    edges.push({
      source: parentId,
      target: node.id,
      data: {},
    });
  }

  if (node.children) {
    for (const child of node.children) {
      const sub = flattenTree(child, node.id);
      nodes.push(...(sub.nodes ?? []));
      edges.push(...(sub.edges ?? []));
    }
  }

  return { nodes, edges };
}

// ── Callback helpers ──

/** Extract label from G6 node/edge datum (v5 callback shape). */
function getLabel(d: Record<string, unknown> | undefined): string {
  if (!d) return "";
  const data = d["data"] as Record<string, unknown> | undefined;
  return String(data?.["label"] ?? d["id"] ?? "");
}

// ── G6 configuration ──

function createMindmapConfig(
  container: HTMLElement,
  tree: G6TreeNode,
): ConstructorParameters<typeof G6Graph>[0] {
  const { nodes, edges } = flattenTree(tree);

  return {
    container,
    width: container.clientWidth || 800,
    height: 500,
    data: { nodes, edges },
    layout: {
      type: "mindmap",
      direction: "H",
      getHeight: () => 36,
      getWidth: (d: Record<string, unknown>) => {
        const label = getLabel(d);
        return Math.max(80, label.length * 12 + 40);
      },
      getVGap: () => 12,
      getHGap: () => 60,
    },
    node: {
      type: "rect",
      style: {
        radius: 8,
        fill: "#f0f5ff",
        stroke: "#4C7CF0",
        lineWidth: 1.5,
        labelText: getLabel,
        labelFill: "#1a1a2e",
        labelFontSize: 13,
        labelPlacement: "center",
        size: (d: Record<string, unknown>) => {
          const len = getLabel(d).length;
          return [Math.max(80, len * 12 + 40), 36];
        },
      },
      state: {
        hover: { fill: "#e6f0ff", stroke: "#2a5cf0", lineWidth: 2 },
      },
    },
    edge: {
      type: "cubic-horizontal",
      style: {
        stroke: "#b0c4de",
        lineWidth: 1.5,
        endArrow: false,
      },
    },
    behaviors: ["zoom-canvas", "drag-canvas", "drag-element"],
    animation: true,
  };
}

function createGraphConfig(
  container: HTMLElement,
  data: G6GraphData,
): ConstructorParameters<typeof G6Graph>[0] {
  return {
    container,
    width: container.clientWidth || 800,
    height: 500,
    data: {
      nodes: data.nodes.map((n) => ({ id: n.id, data: n.data })),
      edges: data.edges.map((e) => ({ source: e.source, target: e.target, data: e.data })),
    },
    layout: {
      type: "force",
      preventOverlap: true,
      nodeStrength: -300,
      edgeStrength: 0.3,
      linkDistance: 150,
    },
    node: {
      type: "circle",
      style: {
        r: 28,
        fill: "#f0f5ff",
        stroke: "#4C7CF0",
        lineWidth: 1.5,
        labelText: getLabel,
        labelFill: "#1a1a2e",
        labelFontSize: 12,
        labelPlacement: "center",
      },
      state: {
        hover: { fill: "#e6f0ff", stroke: "#2a5cf0", lineWidth: 2 },
      },
    },
    edge: {
      style: {
        stroke: "#b0c4de",
        lineWidth: 1.5,
        endArrow: true,
        labelText: (d: Record<string, unknown>) => {
          const data = d["data"] as Record<string, unknown> | undefined;
          return String(data?.["label"] ?? "");
        },
        labelFontSize: 11,
        labelFill: "#666",
      },
    },
    behaviors: ["zoom-canvas", "drag-canvas", "drag-element"],
    animation: true,
  };
}

// ── Public API ──

export async function mountGraph(
  container: HTMLElement,
  type: GraphDSLType,
  treeOrData: G6TreeNode | G6GraphData,
): Promise<void> {
  // Destroy existing graph on this element if any
  unmountGraph(container);

  const config =
    type === "mindmap"
      ? createMindmapConfig(container, treeOrData as G6TreeNode)
      : createGraphConfig(container, treeOrData as G6GraphData);

  const graph = new G6Graph(config);
  await graph.render();

  liveGraphs.set(container, { graph, container });
}

export function unmountGraph(container: HTMLElement): void {
  const inst = liveGraphs.get(container);
  if (inst) {
    try {
      inst.graph.destroy();
    } catch {
      // ignore destroy errors
    }
    liveGraphs.delete(container);
    container.innerHTML = ""; // clear placeholder content
  }
}

/**
 * Scan a root element for graph placeholders and initialize them.
 * Call this after MarkdownBody patches the DOM.
 *
 * Returns a cleanup function that destroys all graphs in this scan batch.
 */
export async function hydratePlaceholders(root: HTMLElement): Promise<() => void> {
  const placeholders = root.querySelectorAll(PLACEHOLDER_SELECTOR);
  const mounted: HTMLElement[] = [];

  for (const el of placeholders) {
    const ph = el as HTMLElement;
    // Skip already hydrated
    if (liveGraphs.has(ph)) continue;

    const type = ph.getAttribute(ATTR_TYPE) as GraphDSLType | null;
    const raw = ph.getAttribute(ATTR_RAW);
    if (!type || !raw) continue;

    try {
      // Defer to dynamic import to avoid circular deps
      const { parseGraphDSL } = await import("./graph-dsl");
      const parsed = parseGraphDSL(type, decodeURIComponent(raw));

      if (parsed.type === "mindmap") {
        await mountGraph(ph, "mindmap", parsed.tree);
      } else {
        await mountGraph(ph, "graph", parsed.data);
      }
      mounted.push(ph);
    } catch (err) {
      console.warn("[graph-renderer] Failed to hydrate placeholder:", err);
      ph.innerHTML = `<div class="g6-error" style="padding:16px;color:#ff6b6b;font-size:13px;">Graph render error: ${err instanceof Error ? err.message : "unknown"}</div>`;
    }
  }

  // Cleanup function: destroy only graphs mounted in this batch
  return () => {
    for (const el of mounted) {
      unmountGraph(el);
    }
  };
}

/**
 * Destroy all live graph instances. Call on major content transitions.
 */
export function destroyAllGraphs(): void {
  for (const [el] of liveGraphs) {
    unmountGraph(el);
  }
}
