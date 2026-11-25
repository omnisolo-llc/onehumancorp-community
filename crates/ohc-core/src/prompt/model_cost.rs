// SPDX-License-Identifier: Apache-2.0
// Copyright (c) OmniSolo LLC.

//! Local BYOK LLM token cost estimator (per 1M tokens in USD).

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelPricing {
    pub input_cost: f64,
    pub output_cost: f64,
    pub cached_cost: f64,
}

#[inline]
pub fn get_pricing(model: &str) -> ModelPricing {
    match model {
        // Anthropic — Claude 3 family
        "claude-3-opus" => ModelPricing {
            input_cost: 15.00,
            output_cost: 75.00,
            cached_cost: 0.0,
        },
        "claude-3-sonnet" => ModelPricing {
            input_cost: 3.00,
            output_cost: 15.00,
            cached_cost: 0.0,
        },
        "claude-3-haiku" => ModelPricing {
            input_cost: 0.25,
            output_cost: 1.25,
            cached_cost: 0.0,
        },
        // Anthropic — Claude 3.5 family
        "claude-3.5-sonnet" | "claude-3-5-sonnet-20241022" | "claude-3-5-sonnet-20240620" => {
            ModelPricing {
                input_cost: 3.00,
                output_cost: 15.00,
                cached_cost: 0.30,
            }
        }
        "claude-3.5-haiku" => ModelPricing {
            input_cost: 0.80,
            output_cost: 4.00,
            cached_cost: 0.08,
        },
        // Anthropic — Claude 3.7 family
        "claude-3.7-sonnet" => ModelPricing {
            input_cost: 3.00,
            output_cost: 15.00,
            cached_cost: 0.30,
        },
        // OpenAI — GPT-4 family
        "gpt-4" => ModelPricing {
            input_cost: 30.00,
            output_cost: 60.00,
            cached_cost: 0.0,
        },
        "gpt-4-turbo" => ModelPricing {
            input_cost: 10.00,
            output_cost: 30.00,
            cached_cost: 0.0,
        },
        "gpt-4o" => ModelPricing {
            input_cost: 5.00,
            output_cost: 15.00,
            cached_cost: 2.50,
        },
        "gpt-4o-mini" => ModelPricing {
            input_cost: 0.15,
            output_cost: 0.60,
            cached_cost: 0.075,
        },
        // OpenAI — GPT-4.1 family
        "gpt-4.1" => ModelPricing {
            input_cost: 2.00,
            output_cost: 8.00,
            cached_cost: 0.0,
        },
        "gpt-4.1-mini" => ModelPricing {
            input_cost: 0.40,
            output_cost: 1.60,
            cached_cost: 0.0,
        },
        "gpt-4.1-nano" => ModelPricing {
            input_cost: 0.10,
            output_cost: 0.40,
            cached_cost: 0.0,
        },
        // OpenAI — o-series reasoning models
        "o1" | "o1-preview" => ModelPricing {
            input_cost: 15.00,
            output_cost: 60.00,
            cached_cost: 7.50,
        },
        "o1-mini" => ModelPricing {
            input_cost: 3.00,
            output_cost: 12.00,
            cached_cost: 1.50,
        },
        "o3-mini" => ModelPricing {
            input_cost: 1.10,
            output_cost: 4.40,
            cached_cost: 0.55,
        },
        // DeepSeek family
        "deepseek-chat" | "deepseek-v3" => ModelPricing {
            input_cost: 0.14,
            output_cost: 0.28,
            cached_cost: 0.014,
        },
        "deepseek-reasoner" | "deepseek-r1" => ModelPricing {
            input_cost: 0.55,
            output_cost: 2.19,
            cached_cost: 0.14,
        },
        // Meta Llama 3/3.1/3.2/3.3 family via API providers
        "llama-3.3-70b-versatile" | "llama-3.1-8b-instant" | "llama3-8b-8192" => ModelPricing {
            input_cost: 0.05,
            output_cost: 0.08,
            cached_cost: 0.0,
        },
        "llama-3.1-70b-versatile" | "llama3-70b-8192" => ModelPricing {
            input_cost: 0.15,
            output_cost: 0.20,
            cached_cost: 0.0,
        },
        "llama-3.1-405b-reasoning" => ModelPricing {
            input_cost: 2.70,
            output_cost: 2.70,
            cached_cost: 0.0,
        },
        // xAI — Grok family
        "grok-3" | "grok-2" => ModelPricing {
            input_cost: 2.00,
            output_cost: 10.00,
            cached_cost: 0.0,
        },
        "grok-3-mini" | "grok-2-mini" => ModelPricing {
            input_cost: 0.20,
            output_cost: 1.00,
            cached_cost: 0.0,
        },
        // Gemini 1.5 family
        "gemini-1.5-pro" => ModelPricing {
            input_cost: 3.50,
            output_cost: 10.50,
            cached_cost: 0.0,
        },
        "gemini-1.5-flash" => ModelPricing {
            input_cost: 0.35,
            output_cost: 1.05,
            cached_cost: 0.0,
        },
        // Gemini 2.0 family
        "gemini-2.0-flash" => ModelPricing {
            input_cost: 0.10,
            output_cost: 0.40,
            cached_cost: 0.0,
        },
        "gemini-2.0-flash-lite" => ModelPricing {
            input_cost: 0.075,
            output_cost: 0.30,
            cached_cost: 0.0,
        },
        // Gemini 2.5 family
        "gemini-2.5-pro" => ModelPricing {
            input_cost: 1.25,
            output_cost: 10.00,
            cached_cost: 0.0,
        },
        "gemini-2.5-flash" => ModelPricing {
            input_cost: 0.15,
            output_cost: 0.60,
            cached_cost: 0.0,
        },
        // MiniMax — M2.7 family
        "minimax-m2.7" => ModelPricing {
            input_cost: 1.00,
            output_cost: 1.00,
            cached_cost: 0.0,
        },
        "minimax-m2.7-turbo" => ModelPricing {
            input_cost: 0.50,
            output_cost: 0.50,
            cached_cost: 0.0,
        },
        // Local zero-cost inference
        m if m.contains("ollama") || m.contains("local") => ModelPricing {
            input_cost: 0.0,
            output_cost: 0.0,
            cached_cost: 0.0,
        },
        // Standard fallback for unrecognized models
        _ => ModelPricing {
            input_cost: 3.00,
            output_cost: 15.00,
            cached_cost: 1.50,
        },
    }
}

#[inline]
pub fn calculate_cost_cents(
    model: &str,
    input_tokens: i64,
    output_tokens: i64,
    cached_input_tokens: i64,
) -> i64 {
    let cost = calculate_cost(model, input_tokens, output_tokens, cached_input_tokens);
    (cost * 100.0).round() as i64
}

#[inline]
pub fn calculate_cost(
    model: &str,
    input_tokens: i64,
    output_tokens: i64,
    cached_input_tokens: i64,
) -> f64 {
    let pricing = get_pricing(model);

    let input_cost = (input_tokens as f64 / 1_000_000.0) * pricing.input_cost;
    let output_cost = (output_tokens as f64 / 1_000_000.0) * pricing.output_cost;
    let cached_cost = (cached_input_tokens as f64 / 1_000_000.0) * pricing.cached_cost;

    let total = input_cost + output_cost + cached_cost;
    (total * 10000.0).round() / 10000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_model_cost_estimation() {
        assert_eq!(
            calculate_cost("claude-3-opus", 1_000_000, 1_000_000, 0),
            90.0
        );
        assert_eq!(calculate_cost("gpt-4o-mini", 1_000_000, 1_000_000, 0), 0.75);
        assert_eq!(
            calculate_cost("gpt-4o", 1_000_000, 1_000_000, 1_000_000),
            22.50
        );
        assert_eq!(
            calculate_cost_cents("gpt-4o", 1_000_000, 1_000_000, 1_000_000),
            2250
        );
        assert_eq!(
            calculate_cost("ollama-llama3", 1_000_000, 1_000_000, 1_000_000),
            0.0
        );
        assert_eq!(
            calculate_cost("local-model", 1_000_000, 1_000_000, 1_000_000),
            0.0
        );
        assert_eq!(
            calculate_cost("unknown-model", 1_000_000, 1_000_000, 1_000_000),
            19.50
        );
    }
}
