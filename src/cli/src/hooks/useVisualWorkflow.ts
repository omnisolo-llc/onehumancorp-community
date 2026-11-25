// SPDX-License-Identifier: Apache-2.0
import { useState } from 'react';
import { resolveCliConfig } from '../config.js';
import { submitAndAwaitTask } from '../taskCenterClient.js';

// Wire format matches the Rust internally tagged NodeType enum.
export type NodeType =
  | { type: 'Input'; name: string }
  | { type: 'Llm'; prompt_template: string }
  | { type: 'Output' }
  | { type: 'Tool'; tool_name: string; args_template: string }
  | { type: 'Condition'; condition_expression: string; true_target: string; false_target: string }
  | { type: 'SubAgent'; agent_name: string; task_template: string }
  | { type: 'HumanInLoop'; prompt_template: string }
  | { type: 'Merge' | 'ParallelJoin'; state_keys: string[]; output_key: string }
  | { type: 'ParallelFork'; targets: string[] };

export interface Node {
  id: string;
  node_type: NodeType;
}

export interface Edge {
  source: string;
  target: string;
}

export interface WorkflowGraph {
  nodes: Node[];
  edges: Edge[];
}

export const useVisualWorkflow = () => {
  const [status, setStatus] = useState<'idle' | 'running' | 'complete' | 'error'>('idle');
  const [result, setResult] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const runWorkflow = async (graph: WorkflowGraph, inputs: Record<string, string>) => {
    setStatus('running');
    setError(null);
    setResult(null);

    try {
      const config = resolveCliConfig();
      const prompt = JSON.stringify({
        workflow_graph: graph,
        inputs,
      });
      const outcome = await submitAndAwaitTask(config, {
        prompt,
        requested_action: 'run_workflow',
      });

      setResult(outcome.output);
      setStatus('complete');
    } catch (err: unknown) {
      setError(
        err instanceof Error && err.message ? err.message : 'An error occurred during execution.',
      );
      setStatus('error');
    }
  };

  return { status, result, error, runWorkflow };
};
