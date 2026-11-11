/** What a secret value shows instead in a planned statement (as the Redis config preview masks it). */
export const SECRET_MASK = '********';

/**
 * The planned statements of a variable change as they are shown: for a
 * variable that holds a secret, the new value after `name =` is masked
 * (`SET GLOBAL name = '********'`). A reset (`= DEFAULT`, `RESET name`) and
 * statements without a value (the configuration reload) stay as they are.
 * Server variable names never contain ` = `, so the first one ends the name.
 */
export function shownStatements(statements: readonly string[], secret: boolean): string[] {
  if (!secret) return [...statements];
  return statements.map((statement) => {
    const at = statement.indexOf(' = ');
    if (at < 0) return statement;
    const value = statement.slice(at + 3).trim();
    if (value.toUpperCase() === 'DEFAULT') return statement;
    return `${statement.slice(0, at + 3)}'${SECRET_MASK}'`;
  });
}

/** From this length on, a cut-off start of the secret is masked too (servers shorten long values in their messages). */
const MIN_MASKED_PREFIX = 16;

function escapeRegExp(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/**
 * A server message about a secret variable as it is shown: the server
 * repeats the value it refused (MySQL and MariaDB: "can't be set to the
 * value of '...'", PostgreSQL: `invalid value for parameter "x": "..."`), so
 * every occurrence of the typed `secret` becomes the mask. A value of one or
 * two characters is masked only inside quotes, where the servers put it:
 * masking it everywhere would garble the message. When the server cut a long
 * value short, the longest start of it that the message holds is masked.
 */
export function maskSecret(message: string, secret: string): string {
  if (secret === '') return message;
  if (secret.length < 3) {
    return message.replace(new RegExp(`(['"\`])${escapeRegExp(secret)}\\1`, 'g'), `$1${SECRET_MASK}$1`);
  }
  let shown = message.split(secret).join(SECRET_MASK);
  for (let length = secret.length - 1; length >= MIN_MASKED_PREFIX; length -= 1) {
    const start = secret.slice(0, length);
    if (shown.includes(start)) {
      shown = shown.split(start).join(SECRET_MASK);
      break;
    }
  }
  return shown;
}
