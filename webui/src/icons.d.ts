// unplugin-icons 的默认 solid 声明引用 solid-js 的 JSX 命名空间,与
// @solidjs/web(Solid 2 rc 的 JSX 运行时)不匹配,导致组件返回类型不可赋值。
// 这里改为与本项目 JSX 运行时一致的声明(不引入 unplugin-icons/types/solid)。
declare module "~icons/*" {
  const component: (props: Record<string, unknown>) => SVGSVGElement;
  export default component;
}
