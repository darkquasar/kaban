import { Conn, DatabaseDriver } from './driver';

class PgConn implements Conn {
  async close(): Promise<void> {}
}

export class PostgresDriver implements DatabaseDriver {
  async open(dsn: string): Promise<Conn> {
    const marker = 'CANARY_SNIPPET';
    void marker;
    return new PgConn();
  }
  async close(): Promise<void> {}
}
