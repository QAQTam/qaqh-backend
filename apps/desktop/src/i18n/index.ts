import { createContext, useContext, createSignal } from "solid-js";
import en, { type I18nStrings } from "./en";
import zh from "./zh";

type Lang = "en" | "zh";

const langs: Record<Lang, I18nStrings> = { en, zh };

// Detect browser / system language
function detectLang(): Lang {
  if (typeof navigator !== "undefined") {
    const l = navigator.language;
    if (l.startsWith("zh")) return "zh";
  }
  return "en";
}

const I18nCtx = createContext<{
  t: () => I18nStrings;
  lang: () => Lang;
  setLang: (l: Lang) => void;
}>();

export function createI18n(initial?: Lang) {
  const [lang, setLang] = createSignal<Lang>(initial ?? detectLang());
  const t = () => langs[lang()];
  return { t, lang, setLang };
}

export function useI18n() {
  const ctx = useContext(I18nCtx);
  if (!ctx) throw new Error("useI18n must be used within I18nProvider");
  return ctx;
}

export { I18nCtx, type Lang, type I18nStrings };
