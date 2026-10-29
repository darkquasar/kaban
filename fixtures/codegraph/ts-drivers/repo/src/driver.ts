export interface Conn {
  close(): Promise<void>;
}

/** DatabaseDriver opens connections. CANARY_DOC */
export interface DatabaseDriver {
  open(dsn: string): Promise<Conn>;
  close(): Promise<void>;
}
