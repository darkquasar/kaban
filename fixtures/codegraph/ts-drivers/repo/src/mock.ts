import { Conn } from './driver';

class MockConn implements Conn {
  async close(): Promise<void> {}
}

/** Structurally compatible with DatabaseDriver, no `implements`. */
export class MockDriver {
  open(dsn: string): Promise<Conn> {
    return Promise.resolve(new MockConn());
  }
  close(): Promise<void> {
    return Promise.resolve();
  }
}
