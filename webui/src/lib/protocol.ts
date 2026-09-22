export const RINGING_SCHEMA = "qaqh.Ringing";
export const RINGING_VERSION = 1;

/** Streaming-markdown token ids (subset we render), re-declared for typing. */
export const MD = {
  PARAGRAPH: 2,
  HEADING_1: 3,
  HEADING_2: 4,
  HEADING_3: 5,
  HEADING_4: 6,
  HEADING_5: 7,
  HEADING_6: 8,
  CODE_BLOCK: 9,
  CODE_FENCE: 10,
  CODE_INLINE: 11,
  ITALIC: 12,
  STRONG: 14,
  STRIKE: 16,
  LINK: 17,
  IMAGE: 19,
  BLOCKQUOTE: 20,
  LINE_BREAK: 21,
  RULE: 22,
  LIST_UNORDERED: 23,
  LIST_ORDERED: 24,
  LIST_ITEM: 25,
  CHECKBOX: 26,
  TABLE: 27,
  TABLE_ROW: 28,
  TABLE_CELL: 29,
} as const;
