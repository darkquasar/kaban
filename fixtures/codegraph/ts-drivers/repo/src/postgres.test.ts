import { PostgresDriver } from './postgres';

declare function test(name: string, fn: () => unknown): void;

test('open', () => new PostgresDriver().open('x'));
