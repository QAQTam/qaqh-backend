// SolidJS 2.x tsc compatibility patches.
// tsc handles children differently from what dom-expressions expects;
// this file prevents false-positive type errors on intrinsic elements.
import type { JSX as SolidJSX } from "@solidjs/web";

declare global {
  namespace JSX {
    // Allow children to be Element or Element[] on all intrinsic elements.
    interface IntrinsicElements {
      [elemName: string]: {
        children?: SolidJSX.Element | SolidJSX.Element[] | undefined;
        ref?: any;
      };
    }
  }
}
