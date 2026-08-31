import {
  createContext,
  useContext,
  useState,
  useCallback,
  useEffect,
  type ReactNode,
} from 'react';
import React from 'react';
import {
  getToken as readToken,
  setToken as writeToken,
  clearToken as removeToken,
  isAuthenticated as checkAuth,
} from '../lib/auth';
import {
  pair as apiPair,
  getPublicHealth,
  getMornevenSession,
  loginMorneven,
  type MornevenAuthUser,
} from '../lib/api';

export interface AuthState {
  token: string | null;
  isAuthenticated: boolean;
  authMode: 'pairing' | 'morneven';
  user: MornevenAuthUser | null;
  requiresPairing: boolean;
  loading: boolean;
  login: (email: string, password: string) => Promise<void>;
  pair: (code: string) => Promise<void>;
  logout: () => void;
}

const AuthContext = createContext<AuthState | null>(null);

export interface AuthProviderProps {
  children: ReactNode;
}

export function AuthProvider({ children }: AuthProviderProps) {
  const [token, setTokenState] = useState<string | null>(readToken);
  const [authenticated, setAuthenticated] = useState<boolean>(false);
  const [authMode, setAuthMode] = useState<'pairing' | 'morneven'>('pairing');
  const [user, setUser] = useState<MornevenAuthUser | null>(null);
  const [requiresPairing, setRequiresPairing] = useState<boolean>(true);
  const [loading, setLoading] = useState<boolean>(true);

  useEffect(() => {
    let cancelled = false;

    const bootstrap = async () => {
      try {
        const health = await getPublicHealth();
        if (cancelled) return;
        const mode = health.auth_mode === 'morneven' ? 'morneven' : 'pairing';
        setAuthMode(mode);

        if (mode === 'morneven') {
          setRequiresPairing(true);
          if (!checkAuth()) {
            setAuthenticated(false);
            setUser(null);
            return;
          }
          try {
            const session = await getMornevenSession();
            if (cancelled) return;
            setAuthenticated(true);
            setUser(session.user);
          } catch {
            if (cancelled) return;
            removeToken();
            setTokenState(null);
            setAuthenticated(false);
            setUser(null);
          }
          return;
        }

        setRequiresPairing(health.require_pairing);
        setAuthenticated(!health.require_pairing || checkAuth());
        setUser(null);
      } catch {
        if (!cancelled) {
          setAuthenticated(false);
          setUser(null);
        }
      } finally {
        if (!cancelled) setLoading(false);
      }
    };

    void bootstrap();
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    const handler = (e: StorageEvent) => {
      if (e.key !== 'zeroclaw_token') return;
      const nextToken = readToken();
      setTokenState(nextToken);
      if (!nextToken) {
        setAuthenticated(false);
        setUser(null);
        return;
      }
      if (authMode === 'pairing') {
        setAuthenticated(true);
        return;
      }
      void getMornevenSession()
        .then((session) => {
          setAuthenticated(true);
          setUser(session.user);
        })
        .catch(() => {
          removeToken();
          setTokenState(null);
          setAuthenticated(false);
          setUser(null);
        });
    };
    window.addEventListener('storage', handler);
    return () => window.removeEventListener('storage', handler);
  }, [authMode]);

  const login = useCallback(async (email: string, password: string): Promise<void> => {
    const next = await loginMorneven(email, password);
    writeToken(next.token);
    setTokenState(next.token);
    setAuthMode('morneven');
    setUser(next.user);
    setAuthenticated(true);
  }, []);

  const pair = useCallback(async (code: string): Promise<void> => {
    const { token: newToken } = await apiPair(code);
    writeToken(newToken);
    setTokenState(newToken);
    setAuthMode('pairing');
    setUser(null);
    setAuthenticated(true);
  }, []);

  const logout = useCallback((): void => {
    removeToken();
    setTokenState(null);
    setUser(null);
    setAuthenticated(false);
  }, []);

  const value: AuthState = {
    token,
    isAuthenticated: authenticated,
    authMode,
    user,
    requiresPairing,
    loading,
    login,
    pair,
    logout,
  };

  return React.createElement(AuthContext.Provider, { value }, children);
}

export function useAuth(): AuthState {
  const ctx = useContext(AuthContext);
  if (!ctx) {
    throw new Error('useAuth must be used within an <AuthProvider>');
  }
  return ctx;
}
