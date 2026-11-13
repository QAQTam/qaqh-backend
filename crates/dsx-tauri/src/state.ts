export let execLiveOutput: Record<string, string> = {}
export let toolResults: Record<string, { content: string; success: boolean }> = {}

export function clearToolOutputs() {
  execLiveOutput = {}
  toolResults = {}
}
