import { IS_DEV, SOME_VALUE } from './other.constants'

if (SOME_VALUE === 'x') {
  console.log('x')
} else {
  console.log('dead')
}

if (IS_DEV) {
  console.log('x')
} else {
  console.log('dead')
}
