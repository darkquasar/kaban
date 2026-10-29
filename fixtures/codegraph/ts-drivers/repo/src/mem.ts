import { Conn, DatabaseDriver } from './driver';

class MemConn implements Conn {
  async close(): Promise<void> {}
}

export class MemDriver implements DatabaseDriver {
  async open(dsn: string): Promise<Conn> {
    return new MemConn();
  }
  async close(): Promise<void> {}
}
