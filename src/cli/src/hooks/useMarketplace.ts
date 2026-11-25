// SPDX-License-Identifier: Apache-2.0
import { useState, useEffect } from 'react';
import { resolveCliConfig } from '../config.js';
import { searchMarketplaceAgents, type MarketplaceAgent } from '../taskCenterClient.js';

export type AgentTemplate = MarketplaceAgent;

export const useMarketplace = () => {
  const [agents, setAgents] = useState<AgentTemplate[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const fetchAgents = async (query: string = '') => {
    setLoading(true);
    setError(null);

    try {
      const config = resolveCliConfig();
      const results = await searchMarketplaceAgents(config, query);
      setAgents(results);
    } catch (err: unknown) {
      setError(
        err instanceof Error && err.message
          ? err.message
          : 'Failed to fetch marketplace agents.',
      );
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void fetchAgents();
  }, []);

  return { agents, loading, error, fetchAgents };
};
