// SPDX-License-Identifier: Apache-2.0
"use client";

import { useRouter, useSearchParams } from "next/navigation";
import { Suspense, type FormEvent, useEffect, useState } from "react";
import { PublicAuthShell } from "@/app/components/PublicAuthShell";
import { LoginExtensionsSlot } from "@/components/ExtensionProviders";
import { notifyQueueIdentityChange } from "@/lib/sync/queueIdentity";
import { safeReturnPath } from "@/lib/auth/url";

const GENERIC_ERROR = "We couldn't sign you in. Check your details and try again.";
const MISSING_CREDENTIALS_ERROR = "Please enter your email or username and password.";
const MISSING_IDENTIFIER_ERROR = "Please enter your email or username.";
const MISSING_PASSWORD_ERROR = "Please enter your password.";

function LoginForm() {
  const router = useRouter();
  const searchParams = useSearchParams();
  const [identifier, setIdentifier] = useState("");
  const [password, setPassword] = useState("");
  const [organization, setOrganization] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const next = safeReturnPath(searchParams.get("next"));
  const testTenant = searchParams.get("test_tenant");

  useEffect(() => {
    if (testTenant && typeof window !== "undefined") {
      localStorage.setItem("tenant_id", testTenant);
      localStorage.setItem("tenant", testTenant);
      localStorage.setItem("business_display_name", testTenant);
      document.cookie = `tenant_id=${encodeURIComponent(testTenant)}; path=/; max-age=86400; SameSite=Lax`;
    }
  }, [testTenant]);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const trimmedIdentifier = identifier.trim();
    if (!trimmedIdentifier && !password) {
      setError(MISSING_CREDENTIALS_ERROR);
      return;
    }
    if (!trimmedIdentifier) {
      setError(MISSING_IDENTIFIER_ERROR);
      return;
    }
    if (!password) {
      setError(MISSING_PASSWORD_ERROR);
      return;
    }
    setPending(true);
    setError(null);
    notifyQueueIdentityChange();
    try {
      const response = await fetch(`/api/v1/auth/login?next=${encodeURIComponent(next)}`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          username: identifier,
          password,
          ...(organization.trim() === "" ? {} : { organization_id: organization }),
        }),
      });
      const body = await response.json();
      if (!response.ok || typeof body?.next !== "string") throw new Error("login denied");
      router.replace(safeReturnPath(body.next));
    } catch {
      setError(GENERIC_ERROR);
    } finally {
      notifyQueueIdentityChange();
      setPending(false);
    }
  }

  return (
    <PublicAuthShell>
      <div aria-labelledby="login-title">
        <div className="mb-8 text-center">
          <h1 id="login-title" className="text-3xl font-bold text-[#1D1D1F] dark:text-[#F5F5F7]">
            Sign in to OmniSolo OneHumanCorp
          </h1>
          <h2 className="text-xl font-semibold text-gray-700 dark:text-gray-300 mt-2">
            Login
          </h2>
          <p className="mt-2 text-sm text-gray-600 dark:text-gray-300">
            Access your business workspace securely.
          </p>
        </div>

        <form className="flex flex-col gap-5" onSubmit={submit} noValidate>
          <label htmlFor="email" className="flex flex-col gap-2 text-sm font-semibold text-gray-700 dark:text-gray-200">
            Email or username
            <input
              id="email"
              name="email"
              autoComplete="username"
              autoFocus
              className="glassmorphism min-h-[52px] w-full rounded-lg px-4 text-base text-[#1D1D1F] outline-none transition focus-visible:border-[#0066FF] focus-visible:ring-2 focus-visible:ring-[#0066FF]/30 dark:text-[#F5F5F7]"
              disabled={pending}
              maxLength={254}
              onChange={(event) => setIdentifier(event.target.value)}
              placeholder="Email or Username"
              aria-label="Email or username"
              required
              value={identifier}
            />
          </label>

          <label htmlFor="password" className="flex flex-col gap-2 text-sm font-semibold text-gray-700 dark:text-gray-200">
            Password
            <input
              id="password"
              name="password"
              autoComplete="current-password"
              className="glassmorphism min-h-[52px] w-full rounded-lg px-4 text-base text-[#1D1D1F] outline-none transition focus-visible:border-[#0066FF] focus-visible:ring-2 focus-visible:ring-[#0066FF]/30 dark:text-[#F5F5F7]"
              disabled={pending}
              maxLength={1024}
              onChange={(event) => setPassword(event.target.value)}
              placeholder="Password"
              aria-label="Password"
              required
              type="password"
              value={password}
            />
          </label>

          <label htmlFor="organization" className="flex flex-col gap-2 text-sm font-semibold text-gray-700 dark:text-gray-200">
            Organization <span className="font-normal text-gray-500">(optional for standalone)</span>
            <input
              id="organization"
              name="organization"
              autoComplete="organization"
              className="glassmorphism min-h-[52px] w-full rounded-lg px-4 text-base text-[#1D1D1F] outline-none transition focus-visible:border-[#0066FF] focus-visible:ring-2 focus-visible:ring-[#0066FF]/30 dark:text-[#F5F5F7]"
              disabled={pending}
              maxLength={128}
              onChange={(event) => setOrganization(event.target.value)}
              placeholder="Organization"
              aria-label="Organization"
              value={organization}
            />
          </label>

          <div aria-live="polite" className="min-h-6">
            {error && (
              <p role="alert" className="rounded-lg border border-red-300 bg-red-50 px-4 py-3 text-sm text-red-800 dark:border-red-900 dark:bg-red-950/40 dark:text-red-200">
                {error}
              </p>
            )}
          </div>

          <div className="flex flex-col gap-2">
            <button
              className="min-h-[54px] w-full rounded-lg bg-[#1D1D1F] p-4 font-bold text-white shadow-[0_4px_14px_0_rgba(0,0,0,0.3)] transition hover:bg-black active:scale-[0.99] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[#0066FF] focus-visible:ring-offset-2 disabled:cursor-wait disabled:opacity-70 dark:bg-white dark:text-[#1D1D1F] dark:hover:bg-gray-200"
              disabled={pending}
              type="submit"
            >
              {pending ? "Signing in…" : "Log in"}
            </button>
            <button
              className="text-xs text-gray-600 hover:text-gray-900 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[#0066FF] rounded dark:text-gray-300 py-1 text-center"
              disabled={pending}
              type="submit"
            >
              Sign in
            </button>
          </div>
        </form>

        <LoginExtensionsSlot next={next} onAuthNavigation={notifyQueueIdentityChange} />

        <p className="mt-6 text-center text-sm text-gray-600 dark:text-gray-300">
          Need an account? <a className="font-semibold text-[#0066FF]" href="/register">Check registration</a>
        </p>
        <div className="mt-3 text-center">
          <a role="button" className="text-xs text-gray-500 hover:text-[#0066FF]" href="/onboarding">Start Business Setup</a>
        </div>
      </div>
    </PublicAuthShell>
  );
}

export default function Login() {
  return (
    <Suspense
      fallback={
        <PublicAuthShell>
          <p className="text-sm text-gray-600 dark:text-gray-300" role="status">
            Loading sign in…
          </p>
        </PublicAuthShell>
      }
    >
      <LoginForm />
    </Suspense>
  );
}
