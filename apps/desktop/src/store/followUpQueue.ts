import { createSignal } from "solid-js";

export type FollowUpItem = {
  id: string;
  text: string;
  files: string[];
  /** 图片附件（base64 + mimeType）。流式排队时必须透传，否则附件静默丢失。 */
  imageBlocks?: Array<{ mimeType: string; data: string }>;
};

export function createFollowUpQueue(
  _seed: string,
  send: (
    text: string,
    files: string[],
    imageBlocks?: Array<{ mimeType: string; data: string }>,
  ) => Promise<void>,
) {
  const [items, setItems] = createSignal<FollowUpItem[]>([]);
  let draining = false;
  const enqueue = (
    text: string,
    files: string[] = [],
    imageBlocks?: Array<{ mimeType: string; data: string }>,
  ) => setItems(list => [...list, { id: crypto.randomUUID(), text, files, imageBlocks }]);
  const update = (id: string, text: string) => setItems(list => list.map(item => item.id === id ? { ...item, text } : item));
  const remove = (id: string) => setItems(list => list.filter(item => item.id !== id));
  const clear = () => setItems([]);
  const drainAfterTurnEnd = async ({ hasPendingGate }: { hasPendingGate: boolean }) => {
    if (draining || hasPendingGate || items().length === 0) return;
    draining = true;
    const item = items()[0];
    try { await send(item.text, item.files, item.imageBlocks); setItems(list => list.slice(1)); }
    finally { draining = false; }
  };
  return { items, enqueue, update, remove, clear, drainAfterTurnEnd };
}
