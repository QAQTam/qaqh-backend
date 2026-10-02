/**
 * Markdown → 净化 HTML(spec §2.1/§7.5):marked 解析,DOMPurify 净化,
 * 远程图片降级为文字链接,外链补 target/rel。全局 hook 只注册一次。
 */
import DOMPurify, { type Config } from "dompurify";
import { marked } from "marked";

marked.setOptions({ gfm: true, breaks: false, async: false });

let hooked = false;

function sameOrigin(url: string): boolean {
  try {
    return new URL(url, globalThis.location?.href ?? "http://127.0.0.1").origin === globalThis.location?.origin;
  } catch {
    return false;
  }
}

function ensureHooks(): void {
  if (hooked || typeof document === "undefined") return;
  hooked = true;
  DOMPurify.addHook("afterSanitizeAttributes", (node) => {
    if (node.tagName === "A") {
      const anchor = node as HTMLAnchorElement;
      const href = anchor.getAttribute("href");
      if (!href || !/^https?:/i.test(href)) {
        // 仅允许普通网页链接;其他 scheme 一律去链保留文字。
        anchor.removeAttribute("href");
      } else {
        anchor.setAttribute("target", "_blank");
        anchor.setAttribute("rel", "noopener noreferrer");
        anchor.setAttribute("referrerpolicy", "no-referrer");
      }
      return;
    }
    if (node.tagName === "IMG") {
      const image = node as HTMLImageElement;
      const src = image.getAttribute("src") ?? "";
      if (src && sameOrigin(src)) return; // 网关同源图片(如附件)放行
      // 远程图片默认不加载 → 渲染为带地址的文字链接(spec §2.2)。
      const link = document.createElement("a");
      if (/^https?:/i.test(src)) {
        link.href = src;
        link.target = "_blank";
        link.rel = "noopener noreferrer";
      }
      link.textContent = src || "图片";
      link.className = "md-image-link";
      node.replaceWith(link);
    }
  });
}

const PURIFY_CONFIG: Config = {
  // 默认白名单已覆盖 GFM 输出;显式去掉脚本类面与表单。
  FORBID_TAGS: ["style", "form", "input", "button", "iframe", "object", "embed"],
  FORBID_ATTR: ["style", "srcset"],
  ALLOW_ARIA_ATTR: true,
};

/** 解析 + 净化;输出字符串只经 innerHTML 写入已净化容器。 */
export function renderMarkdownHtml(src: string): string {
  ensureHooks();
  if (src.trim() === "") return "";
  const html = marked.parse(src) as string;
  return DOMPurify.sanitize(html, PURIFY_CONFIG) as string;
}
