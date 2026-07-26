/**
 * DSL parsers that convert markdown code block text into G6 v5 graph data.
 *
 * Mindmap format (indent-based):
 *   root label
 *     child1 label
 *       grandchild label
 *     child2 label
 *
 * Graph format (edge-list):
 *   NodeA -> NodeB [label="knows", weight=5]
 *   NodeB -> NodeC
 */

// ── G6 v5 data shapes ──

export interface G6TreeNode {
  id: string;
  data: { label: string; [key: string]: unknown };
  children?: G6TreeNode[];
  [key: string]: unknown;
}

export interface G6GraphNode {
  id: string;
  data: { label: string; [key: string]: unknown };
  [key: string]: unknown;
}

export interface G6GraphEdge {
  source: string;
  target: string;
  data: { label?: string; [key: string]: unknown };
  [key: string]: unknown;
}

export interface G6GraphData {
  nodes: G6GraphNode[];
  edges: G6GraphEdge[];
  [key: string]: unknown;
}

// ── Mindmap parser ──

interface RawMindmapLine {
  indent: number;
  label: string;
}

function parseMindmapLines(text: string): RawMindmapLine[] {
  const lines: RawMindmapLine[] = [];
  for (const raw of text.split("\n")) {
    const line = raw.trimEnd();
    if (line.trim() === "") continue;
    const indent = line.length - line.trimStart().length;
    const label = line.trim();
    lines.push({ indent, label });
  }
  return lines;
}

let _mindmapIdCounter = 0;

function buildMindmapTree(lines: RawMindmapLine[]): G6TreeNode {
  _mindmapIdCounter = 0;
  const root: G6TreeNode = { id: `n${_mindmapIdCounter++}`, data: { label: "" }, children: [] };
  const stack: { node: G6TreeNode; indent: number }[] = [{ node: root, indent: -2 }];

  for (const line of lines) {
    const newNode: G6TreeNode = {
      id: `n${_mindmapIdCounter++}`,
      data: { label: line.label },
      children: [],
    };

    // Pop stack until we find the parent (closest ancestor with indent < current)
    while (stack.length > 1 && stack[stack.length - 1]!.indent >= line.indent) {
      stack.pop();
    }

    const parent = stack[stack.length - 1]!.node;
    if (!parent.children) parent.children = [];
    parent.children.push(newNode);

    stack.push({ node: newNode, indent: line.indent });
  }

  // If root has exactly one child, promote it to root
  if (root.children?.length === 1) {
    return root.children[0]!;
  }
  // If root is empty, return first child as root
  if (root.data.label === "" && root.children && root.children.length > 0) {
    const promoted = root.children[0]!;
    if (root.children.length > 1) {
      // Multiple top-level items: promote first, add rest as its siblings under a virtual root
      promoted.children = [...(promoted.children || []), ...root.children.slice(1)];
    }
    return promoted;
  }
  return root;
}

export function parseMindmap(text: string): G6TreeNode {
  const lines = parseMindmapLines(text);
  if (lines.length === 0) {
    return { id: "empty", data: { label: "(empty)" }, children: [] };
  }
  return buildMindmapTree(lines);
}

// ── Graph (edge-list) parser ──

// Matches: NodeA -> NodeB
// Matches: NodeA -> NodeB [key=value, key2="value with spaces"]
const EDGE_RE = /^(.+?)\s*->\s*(.+?)(?:\s*\[(.+)\])?\s*$/;

interface ParsedEdge {
  source: string;
  target: string;
  attrs: Record<string, string>;
}

function parseEdgeLine(line: string): ParsedEdge | null {
  const m = line.trim().match(EDGE_RE);
  if (!m) return null;

  const source = (m[1] ?? "").trim();
  const target = (m[2] ?? "").trim();
  const attrsStr = m[3] ?? "";

  const attrs: Record<string, string> = {};
  if (attrsStr) {
    // Parse key=value pairs, value may be quoted
    const kvRe = /(\w+)\s*=\s*(?:"([^"]*)"|'([^']*)'|(\S+))/g;
    let kvMatch: RegExpExecArray | null;
    while ((kvMatch = kvRe.exec(attrsStr)) !== null) {
      const key = kvMatch[1]!;
      const value = kvMatch[2] ?? kvMatch[3] ?? kvMatch[4] ?? "";
      attrs[key] = value;
    }
  }

  return { source, target, attrs };
}

export function parseGraph(text: string): G6GraphData {
  const nodeMap = new Map<string, G6GraphNode>();
  const edges: G6GraphEdge[] = [];

  const ensureNode = (id: string, label?: string) => {
    if (!nodeMap.has(id)) {
      nodeMap.set(id, { id, data: { label: label ?? id } });
    }
    return nodeMap.get(id)!;
  };

  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) continue;

    const parsed = parseEdgeLine(line);
    if (!parsed) continue;

    ensureNode(parsed.source);
    ensureNode(parsed.target);

    const edgeLabel = parsed.attrs["label"];
    const extraAttrs: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(parsed.attrs)) {
      if (k !== "label") extraAttrs[k] = isNaN(Number(v)) ? v : Number(v);
    }

    edges.push({
      source: parsed.source,
      target: parsed.target,
      data: {
        ...(edgeLabel !== undefined ? { label: edgeLabel } : {}),
        ...extraAttrs,
      },
    });
  }

  return {
    nodes: Array.from(nodeMap.values()),
    edges,
  };
}

// ── Unified parser ──

export type GraphDSLType = "mindmap" | "graph";

export interface ParsedMindmap {
  type: "mindmap";
  tree: G6TreeNode;
}

export interface ParsedGraph {
  type: "graph";
  data: G6GraphData;
}

export type ParsedGraphDSL = ParsedMindmap | ParsedGraph;

export function parseGraphDSL(type: GraphDSLType, text: string): ParsedGraphDSL {
  switch (type) {
    case "mindmap":
      return { type: "mindmap", tree: parseMindmap(text) };
    case "graph":
      return { type: "graph", data: parseGraph(text) };
  }
}
