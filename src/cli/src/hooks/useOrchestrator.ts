// SPDX-License-Identifier: Apache-2.0
import { useState } from 'react';
import type { ToolItem } from '../components/ToolProgress.js';
import { resolveCliConfig } from '../config.js';
import { mapStepsToTools, submitAndAwaitTask } from '../taskCenterClient.js';

export interface OrchestratorState {
  status: string;
  tools: ToolItem[];
  error: string | null;
  runAgent: (message: string) => Promise<void>;
  output: string | null;
}

export const useOrchestrator = (): OrchestratorState => {
  const [status, setStatus] = useState('Idle');
  const [error, setError] = useState<string | null>(null);
  const [tools, setTools] = useState<ToolItem[]>([]);
  const [output, setOutput] = useState<string | null>(null);

  const runAgent = async (message: string) => {
    setStatus('Executing request...');
    setError(null);
    setOutput(null);
    setTools([]);

    try {
      const config = resolveCliConfig();
      const outcome = await submitAndAwaitTask(
        config,
        { prompt: message, requested_action: 'run_task' },
        (progress) => {
          setTools(mapStepsToTools(progress.steps));
          if (progress.status === 'queued') {
            setStatus('Queued...');
          } else if (progress.status === 'running') {
            setStatus('Running...');
          }
        },
      );

      setTools(outcome.tools);
      setOutput(outcome.output);
      setStatus(outcome.statusLabel);
    } catch (err: unknown) {
      setError(
        err instanceof Error && err.message ? err.message : 'An error occurred during execution.',
      );
      setStatus('Error');
    }
  };

  return { status, tools, error, runAgent, output };
};
