import { createSignal, Show, For } from "solid-js";
import type { createFollowUpQueue } from "../../store/followUpQueue";
import ComposerQueue from "./ComposerQueue";
import PermissionLevelSelect from "./PermissionLevelSelect";
import { openImageDialog, readFileBase64, readTextFile, type ImageFileInfo, type TextFileInfo } from "../../runtime/desktopApi";
import { matchSlashCommands, type SlashCommand } from "./slashCommands";

type Queue = ReturnType<typeof createFollowUpQueue>;

interface UploadedImage {
  id: string;
  fileName: string;
  mimeType: string;
  /** We keep the base64 data for sending but use object URL for preview. */
  data: string;
  /** Object URL for <img> preview — avoids passing base64 in UI state. */
  previewUrl: string;
  size: number;
}

interface UploadedText {
  id: string;
  fileName: string;
  content: string;
  size: number;
}

let imageIdCounter = 0;
let textIdCounter = 0;

function makeImageId() { return `img-${++imageIdCounter}-${Date.now()}`; }
function makeTextId() { return `txt-${++textIdCounter}-${Date.now()}`; }

export default function ComposerDock(props: {
  isStreaming: () => boolean;
  hasPendingGate: () => boolean;
  queue: Queue;
  onSend: (text: string, files: string[], imageBlocks?: Array<{ mimeType: string; data: string }>) => Promise<void>;
  onStop: () => Promise<void>;
  mode: string;
  onModeChange: (mode: string) => void;
  model?: string;
  contextTokens?: number;
  contextLimit?: number;
  permissionLevel: number;
  onPermissionLevelChange: (level: number) => void | Promise<void>;
  goalBar?: any;
}) {
  const [text, setText] = createSignal("");
  const [images, setImages] = createSignal<UploadedImage[]>([]);
  const [textFiles, setTextFiles] = createSignal<UploadedText[]>([]);
  const [attachOpen, setAttachOpen] = createSignal(false);
  const [submitError, setSubmitError] = createSignal("");
  const [selectedSlashIndex, setSelectedSlashIndex] = createSignal(0);
  const [dismissedSlashValue, setDismissedSlashValue] = createSignal<string | null>(null);

  function visibleSlashCommands(): readonly SlashCommand[] {
    const value = text();
    return dismissedSlashValue() === value ? [] : matchSlashCommands(value);
  }

  function updateText(value: string): void {
    setText(value);
    setSubmitError("");
    setSelectedSlashIndex(0);
    setDismissedSlashValue(null);
  }

  function selectSlashCommand(command: SlashCommand): void {
    // This first slice provides discovery and keyboard behavior only. Command
    // execution/context registration will be attached to the same catalogue.
    setText(command.command);
    setSelectedSlashIndex(0);
    setDismissedSlashValue(command.command);
  }

  const submit = async () => {
    const value = text().trim();
    const hasImages = images().length > 0;
    const hasTextFiles = textFiles().length > 0;
    if ((!value && !hasImages && !hasTextFiles) || props.hasPendingGate()) return;

    // Build combined text from text files
    let combinedText = value;
    for (const tf of textFiles()) {
      if (tf.content) combinedText += (combinedText ? "\n\n---\n" : "") + tf.content;
    }

    // Build image blocks for the message
    const imageBlocks = images().map(img => ({ mimeType: img.mimeType, data: img.data }));

    setSubmitError("");
    if (props.isStreaming()) {
      props.queue.enqueue(combinedText, []);
    } else {
      try {
        await props.onSend(combinedText, [], imageBlocks.length > 0 ? imageBlocks : undefined);
      } catch (error) {
        setSubmitError(error instanceof Error ? error.message : String(error));
        return;
      }
    }

    // Cleanup preview URLs
    for (const img of images()) URL.revokeObjectURL(img.previewUrl);

    setText("");
    setImages([]);
    setTextFiles([]);
    setAttachOpen(false);
  };

  async function handleUploadImage() {
    try {
      const filePath = await openImageDialog();
      if (!filePath) return;
      const fileName = filePath.split(/[\\/]/).pop() ?? "image";
      const info: ImageFileInfo = await readFileBase64(filePath);
      // Create an object URL from the base64 data for preview
      const byteChars = atob(info.data);
      const byteNums = new Array(byteChars.length);
      for (let i = 0; i < byteChars.length; i++) byteNums[i] = byteChars.charCodeAt(i);
      const byteArr = new Uint8Array(byteNums);
      const blob = new Blob([byteArr], { type: info.mimeType });
      const previewUrl = URL.createObjectURL(blob);
      setImages(prev => [...prev, {
        id: makeImageId(),
        fileName,
        mimeType: info.mimeType,
        data: info.data,
        previewUrl,
        size: info.size,
      }]);
    } catch (e) {
      console.error("Failed to upload image:", e);
    }
    setAttachOpen(false);
  }

  async function handleUploadText() {
    try {
      // Use the generic openDialog for text files
      const { openDialog } = await import("../../runtime/desktopApi");
      const filePath = await openDialog({ title: "选择文本文件" }) as string | null;
      if (!filePath) return;
      const fileName = filePath.split(/[\\/]/).pop() ?? "file";
      const info: TextFileInfo = await readTextFile(filePath);
      setTextFiles(prev => [...prev, {
        id: makeTextId(),
        fileName,
        content: info.content,
        size: info.size,
      }]);
    } catch (e) {
      console.error("Failed to upload text file:", e);
    }
    setAttachOpen(false);
  }

  function removeImage(id: string) {
    setImages(prev => {
      const img = prev.find(i => i.id === id);
      if (img) URL.revokeObjectURL(img.previewUrl);
      return prev.filter(i => i.id !== id);
    });
  }

  function removeTextFile(id: string) {
    setTextFiles(prev => prev.filter(t => t.id !== id));
  }

  const formatSize = (bytes: number) => {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  };

  return <div class="composer-wrap">
    {props.goalBar}
    <ComposerQueue queue={props.queue} />

    {/* Uploaded images preview */}
    <Show when={images().length > 0}>
      <div class="composer-attachments">
        <For each={images()}>
          {(img) => (
            <div class="composer-attachment-image">
              <img src={img.previewUrl} alt={img.fileName} />
              <span class="composer-attachment-name">{img.fileName} ({formatSize(img.size)})</span>
              <button class="composer-attachment-remove" onClick={() => removeImage(img.id)} aria-label="移除图片">×</button>
            </div>
          )}
        </For>
      </div>
    </Show>

    {/* Uploaded text files preview */}
    <Show when={textFiles().length > 0}>
      <div class="composer-attachments">
        <For each={textFiles()}>
          {(tf) => (
            <div class="composer-attachment-text">
              <span class="composer-attachment-icon">📄</span>
              <span class="composer-attachment-name">{tf.fileName} ({formatSize(tf.size)})</span>
              <button class="composer-attachment-remove" onClick={() => removeTextFile(tf.id)} aria-label="移除文本">×</button>
            </div>
          )}
        </For>
      </div>
    </Show>

    <section class="composer-dock" data-composer-dock>
      <textarea value={text()} onInput={event => updateText(event.currentTarget.value)} onKeyDown={event => {
        const commands = visibleSlashCommands();
        if (commands.length > 0) {
          if (event.key === "ArrowDown") {
            event.preventDefault();
            setSelectedSlashIndex(index => (index + 1) % commands.length);
            return;
          }
          if (event.key === "ArrowUp") {
            event.preventDefault();
            setSelectedSlashIndex(index => (index + commands.length - 1) % commands.length);
            return;
          }
          if (event.key === "Escape") {
            event.preventDefault();
            setDismissedSlashValue(text());
            return;
          }
          if (event.key === "Enter" && !event.shiftKey) {
            event.preventDefault();
            selectSlashCommand(commands[selectedSlashIndex() % commands.length]);
            return;
          }
        }
        if (event.key === "Enter" && !event.shiftKey) { event.preventDefault(); void submit(); }
      }} placeholder={props.hasPendingGate() ? "请先处理当前授权请求" : "向 DeepX 提问…"} />
      <Show when={submitError()}>
        {message => <div class="composer-submit-error" role="alert">{message()}</div>}
      </Show>
      <Show when={visibleSlashCommands().length > 0}>
        <div class="composer-slash-menu" role="listbox" aria-label="快捷命令">
          <For each={visibleSlashCommands()}>{(command, index) =>
            <button
              class={["composer-slash-option", { "is-selected": selectedSlashIndex() === index() }]}
              type="button"
              role="option"
              aria-selected={selectedSlashIndex() === index() ? "true" : "false"}
              onClick={() => selectSlashCommand(command)}
            >
              <code>{command.command}</code>
              <span>{command.label}</span>
              <small>{command.description}</small>
            </button>
          }</For>
        </div>
      </Show>
      <footer>
        <div class="composer-controls">
          <div class="composer-attach-wrap" style="position:relative;">
            <button class="composer-attach" aria-label="添加附件" onClick={() => setAttachOpen(p => !p)}>＋</button>
            <Show when={attachOpen()}>
              <div class="composer-attach-menu" style="position:absolute;bottom:100%;left:0;background:var(--bg-secondary);border:var(--panel-border);border-radius:8px;padding:4px;margin-bottom:4px;z-index:10;min-width:140px;box-shadow:var(--shadow-card);">
                <button class="composer-attach-option" onClick={handleUploadImage} style="display:block;width:100%;padding:8px 12px;border:none;background:none;text-align:left;cursor:pointer;border-radius:4px;">🖼️ 上传图片</button>
                <button class="composer-attach-option" onClick={handleUploadText} style="display:block;width:100%;padding:8px 12px;border:none;background:none;text-align:left;cursor:pointer;border-radius:4px;">📄 上传文本</button>
              </div>
            </Show>
          </div>
          <button class="composer-mode" onClick={() => props.onModeChange(props.mode === "plan" ? "code" : "plan")}>{props.mode === "plan" ? "规划" : "执行"}</button>
          <PermissionLevelSelect compact level={props.permissionLevel} onChange={props.onPermissionLevelChange} />
        </div>
        <div class="composer-meta">
          {(props.contextTokens != null || props.contextLimit != null) && <span class="composer-context">{props.contextTokens != null && props.contextLimit != null ? `${(props.contextTokens / 1000).toFixed(1)}K / ${(props.contextLimit / 1000).toFixed(0)}K` : ''}</span>}
          <span>{props.model}</span>
          {props.isStreaming()
            ? <button class="composer-stop" onClick={() => void props.onStop()}>■</button>
            : <button class="composer-send" disabled={(!text().trim() && images().length === 0 && textFiles().length === 0) || props.hasPendingGate()} onClick={() => void submit()}>↑</button>}
        </div>
      </footer>
    </section>
  </div>;
}
