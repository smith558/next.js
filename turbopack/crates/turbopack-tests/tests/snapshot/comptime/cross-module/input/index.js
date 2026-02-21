import { SOME_VALUE, MISSING } from './other.constants'

if (SOME_VALUE === 'x') {
  console.log('x')
} else {
  console.log('dead')
}

if (MISSING === 'x') {
  console.log('x')
} else {
  console.log('dead')
}

console.log(SOME_VALUE)
