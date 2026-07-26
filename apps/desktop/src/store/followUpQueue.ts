import { createSignal } from "solid-js";

export type FollowUpItem = { id: string; text: string; files: string[] };

export function createFollowUpQueue(_seed: string, send: (text: string, files: string[]) => Promise<void>) {
  const [items, setItems] = createSignal<FollowUpItem[]>([]);
  let draining = false;
  const enqueue = (text: string, files: string[] = []) => setItems(list => [...list, { id: crypto.randomUUID(), text, files }]);
  const update = (id: string, text: string) => setItems(list => list.map(item => item.id === id ? { ...item, text } : item));
  const remove = (id: string) => setItems(list => list.filter(item => item.id !== id));
  const clear = () => setItems([]);
  const drainAfterTurnEnd = async ({ hasPendingGate }: { hasPendingGate: boolean }) => {
    if (draining || hasPendingGate || items().length === 0) return;
    draining = true;
    const item = items()[0];
    try { await send(item.text, item.files); setItems(list => list.slice(1)); }
    finally { draining = false; }
  };
  return { items, enqueue, update, remove, clear, drainAfterTurnEnd };
}
