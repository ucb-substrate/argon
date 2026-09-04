import type {ReactNode} from 'react';
import Link from '@docusaurus/Link';
import CodeBlock from '@theme/CodeBlock';

export type ApiParameter = {
  name: string;
  type: string;
  typeHref?: string;
  requirement: string;
  description: ReactNode;
};

type ApiItemProps = {
  id: string;
  name: string;
  signature: string;
  summary: ReactNode;
  children?: ReactNode;
};

function TypeRef({type, href}: {type: string; href?: string}) {
  const code = <code>{type}</code>;
  return href ? <Link to={href}>{code}</Link> : code;
}

export function ApiItem({id, name, signature, summary, children}: ApiItemProps) {
  return (
    <section className="apiItem" data-api-id={id} aria-label={name}>
      <CodeBlock language="argon">{signature}</CodeBlock>
      <p>{summary}</p>
      {children}
    </section>
  );
}

export function ParameterTable({rows}: {rows: ApiParameter[]}) {
  return (
    <div className="apiTableWrap">
      <table className="apiTable">
        <thead>
          <tr>
            <th>Argument</th>
            <th>Type</th>
            <th>Requirement</th>
            <th>Description</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.name}>
              <td><code>{row.name}</code></td>
              <td><TypeRef type={row.type} href={row.typeHref} /></td>
              <td>{row.requirement}</td>
              <td>{row.description}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

export type TypeField = {
  name: string;
  type: string;
  typeHref?: string;
  description: ReactNode;
};

export function FieldTable({rows}: {rows: TypeField[]}) {
  return (
    <div className="apiTableWrap">
      <table className="apiTable">
        <thead>
          <tr>
            <th>Field</th>
            <th>Type</th>
            <th>Description</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.name}>
              <td><code>{row.name}</code></td>
              <td><TypeRef type={row.type} href={row.typeHref} /></td>
              <td>{row.description}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

export function Returns({type, href, children}: {type: string; href?: string; children: ReactNode}) {
  return (
    <p className="apiReturns">
      <strong>Returns</strong> <TypeRef type={type} href={href} />. {children}
    </p>
  );
}
